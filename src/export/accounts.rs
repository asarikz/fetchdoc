//! GnuCash chart-of-accounts loader and LLM-driven debit-account picker.
//!
//! When the user passes `--accounts <path>` to `export gnucash`, we:
//! 1. Parse the file into a list of [`AccountEntry`] (full path + description
//!    + type), e.g. `Expenses:通信費:インターネット`.
//! 2. For each `Document` record, ask Claude to pick the best-matching expense
//!    account for the invoice using the counterparty name, total amount, and
//!    optional T-number.
//! 3. The chart of accounts goes into the **system prompt** with prompt
//!    caching enabled, so repeated calls in one batch reuse the cached prefix.
//!
//! When the model's pick is missing or not in the chart, the caller falls
//! back to `--debit-account` and flips the document's `status` to
//! `needs_review`.
//!
//! ## Supported formats
//!
//! **GnuCash account export (recommended):** the CSV produced by
//! *File → Export → Export Account Tree* in GnuCash. We key off the
//! `Full Account Name` column for the path and use `Description` as extra
//! prompt context. Rows where `Placeholder = T` (organising parents that
//! can't hold transactions) or `Hidden = T` are filtered out automatically.
//!
//! ```csv
//! Type,Full Account Name,Account Name,Account Code,Description,Account Color,Notes,Symbol,Namespace,Hidden,Tax Info,Placeholder
//! EXPENSE,費用:通信費,通信費,,通信に関する費用,,,JPY,CURRENCY,F,F,F
//! EXPENSE,費用:消耗品費,消耗品費,,,,,JPY,CURRENCY,F,F,F
//! ```
//!
//! **Plain text (one account per line):** quick fallback for when the user
//! wants to maintain the list by hand. `#` introduces a comment, blank lines
//! are skipped:
//!
//! ```text
//! Expenses:通信費
//! Expenses:消耗品費  # 文房具、郵送費 etc
//! ```
//!
//! Format is auto-detected from the first non-empty line: if it starts with
//! the GnuCash CSV header (`Type,...,Full Account Name,...`), CSV mode is
//! used; otherwise the file is parsed as plain text.

use crate::anthropic::{Client, UserBlock};
use crate::io::{Document, Extracted};
use anyhow::Context;
use std::collections::HashSet;
use std::path::Path;

const MAX_TOKENS: u32 = 64;

/// One row of the chart. Carries enough info to render a useful prompt line
/// without re-reading the source file.
#[derive(Debug, Clone)]
pub struct AccountEntry {
    /// Fully-qualified account name, e.g. `費用:通信費:インターネット`.
    pub full_name: String,
    /// GnuCash `Description` column, if non-empty. Surfaced to the LLM as
    /// extra context when the account name alone is ambiguous.
    pub description: Option<String>,
    /// GnuCash account type (`EXPENSE`, `ASSET`, `LIABILITY`, ...). `None`
    /// when loaded from the plain-text format. Surfaced to the LLM so it can
    /// distinguish 費用 buckets from capitalised-asset buckets.
    pub account_type: Option<String>,
}

/// In-memory chart of accounts. Order is preserved (used in the prompt) and a
/// `HashSet` over `full_name`s lets us validate model picks in O(1).
#[derive(Debug)]
pub struct Chart {
    accounts: Vec<AccountEntry>,
    set: HashSet<String>,
}

impl Chart {
    /// Load and parse a chart-of-accounts file. See module docs for formats.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading chart of accounts {}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse the text body. Auto-detects GnuCash CSV vs plain-text format.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        if looks_like_gnucash_csv(text) {
            parse_gnucash_csv(text)
        } else {
            parse_plain_text(text)
        }
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.set.contains(name)
    }

    /// Render one entry per line for inclusion in the system prompt. Format:
    /// `[<TYPE>] <full name> — <description>` with bracketed type and `—
    /// description` only included when present.
    fn rendered(&self) -> String {
        let mut out = String::new();
        for entry in &self.accounts {
            if let Some(t) = entry.account_type.as_deref() {
                out.push('[');
                out.push_str(t);
                out.push_str("] ");
            }
            out.push_str(&entry.full_name);
            if let Some(desc) = entry.description.as_deref() {
                out.push_str(" — ");
                out.push_str(desc);
            }
            out.push('\n');
        }
        out
    }
}

/// Heuristic: if the first non-empty/non-comment line starts with `Type,` and
/// also names `Full Account Name`, we're looking at the GnuCash export format.
fn looks_like_gnucash_csv(text: &str) -> bool {
    for raw in text.lines() {
        let trimmed = raw.trim_start_matches('\u{feff}').trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return trimmed.starts_with("Type,") && trimmed.contains("Full Account Name");
    }
    false
}

fn parse_plain_text(text: &str) -> anyhow::Result<Chart> {
    let mut accounts = Vec::new();
    let mut set = HashSet::new();
    for raw in text.lines() {
        let without_comment = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let line = without_comment.trim();
        if line.is_empty() {
            continue;
        }
        if !set.insert(line.to_string()) {
            continue;
        }
        accounts.push(AccountEntry {
            full_name: line.to_string(),
            description: None,
            account_type: None,
        });
    }
    if accounts.is_empty() {
        anyhow::bail!("chart of accounts is empty (only comments / blank lines)");
    }
    Ok(Chart { accounts, set })
}

fn parse_gnucash_csv(text: &str) -> anyhow::Result<Chart> {
    // Strip a UTF-8 BOM if present — GnuCash on some platforms emits one.
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());

    let headers = rdr.headers().context("reading GnuCash CSV header")?.clone();
    let col = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };
    let i_full = col("Full Account Name")
        .ok_or_else(|| anyhow::anyhow!("GnuCash CSV missing 'Full Account Name' column"))?;
    let i_type = col("Type");
    let i_desc = col("Description");
    let i_hidden = col("Hidden");
    let i_placeholder = col("Placeholder");

    let mut accounts = Vec::new();
    let mut set = HashSet::new();
    let mut skipped = 0usize;
    for (row_idx, row) in rdr.records().enumerate() {
        let row = row.with_context(|| format!("reading GnuCash CSV row {}", row_idx + 2))?;
        let Some(full_name) = row.get(i_full).map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        // Drop placeholders (parent buckets that can't hold transactions in
        // GnuCash) and hidden accounts. Both surface as `T` / `F` strings.
        if i_placeholder.and_then(|i| row.get(i)) == Some("T") {
            skipped += 1;
            continue;
        }
        if i_hidden.and_then(|i| row.get(i)) == Some("T") {
            skipped += 1;
            continue;
        }
        if !set.insert(full_name.to_string()) {
            continue;
        }
        let description = i_desc
            .and_then(|i| row.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let account_type = i_type
            .and_then(|i| row.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        accounts.push(AccountEntry {
            full_name: full_name.to_string(),
            description,
            account_type,
        });
    }

    if accounts.is_empty() {
        anyhow::bail!(
            "GnuCash CSV had no usable accounts ({skipped} placeholder/hidden rows skipped)"
        );
    }
    Ok(Chart { accounts, set })
}

/// Outcome of a single picker call. `Picked` means the model returned an
/// account that's present in the chart; the caller can use it as the debit
/// account directly. `Fallback` means it didn't — caller should use the
/// configured `--debit-account` and flag the record for review.
pub enum Pick {
    Picked(String),
    Fallback { reason: String },
}

/// Ask the model to choose a debit account from `chart` for this invoice.
pub async fn pick_debit_account(
    client: &Client,
    chart: &Chart,
    doc: &Document,
    extracted: &Extracted,
) -> anyhow::Result<Pick> {
    let system = build_system_prompt(chart);
    let user = build_user_prompt(doc, extracted);

    let raw = client
        .complete_with_cached_system(&system, &[UserBlock::Text(user)], MAX_TOKENS)
        .await
        .context("calling Anthropic Messages API for account selection")?;

    let pick = parse_pick(&raw);
    Ok(match pick {
        Some(name) if chart.contains(&name) => Pick::Picked(name),
        Some(name) => Pick::Fallback {
            reason: format!("model returned {name:?}, not in chart"),
        },
        None => Pick::Fallback {
            reason: format!("could not parse model reply: {raw}"),
        },
    })
}

fn build_system_prompt(chart: &Chart) -> String {
    format!(
        "You are picking the best-matching GnuCash debit (expense) account for a Japanese invoice or receipt.\n\
\n\
Below is the user's chart of accounts. Each line may include `[TYPE]` (account type, e.g. EXPENSE / ASSET) and a description after `—`. Pick **exactly one** account whose **full name** (the bare colon-separated path, with type/description stripped) best categorises the expense in the user message.\n\
\n\
Rules:\n\
- Reply with a single line: just the full account name, exactly as it appears in the list, with no `[TYPE]` prefix and no `— description` suffix. No prose, no quotes, no markdown fence.\n\
- If no account in the list is a reasonable fit, reply with the single token `NONE`.\n\
- Prefer the most specific (deepest) account when several would fit.\n\
- Capitalised purchases (e.g. equipment over the small-asset threshold) typically belong under an ASSET account; routine costs belong under EXPENSE.\n\
- Account names use `:` as the hierarchy separator and may contain Japanese characters — match them verbatim.\n\
\n\
Available accounts ({n} total):\n\
{accounts}",
        n = chart.len(),
        accounts = chart.rendered(),
    )
}

fn build_user_prompt(doc: &Document, extracted: &Extracted) -> String {
    let mut lines = vec![
        format!("Counterparty: {}", extracted.counterparty_name),
        format!("Total amount: {} JPY", extracted.total_amount_jpy),
        format!("Transaction date: {}", extracted.transaction_date),
    ];
    if let Some(t) = extracted.counterparty_t_number.as_deref() {
        lines.push(format!("T-number: {t}"));
    }
    if let Some(meta) = doc.source_meta.as_ref()
        && let Some(subject) = meta.get("subject").and_then(|v| v.as_str())
    {
        lines.push(format!("Email subject: {subject}"));
    }
    lines.push(String::new());
    lines.push("Pick the best-matching account from the list in the system prompt.".into());
    lines.join("\n")
}

/// Trim whitespace, code fences, and stray quotes from the model's reply.
/// Returns `None` for the literal `NONE` sentinel or empty input.
fn parse_pick(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("```") {
        // Skip a language tag if present.
        let after = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
        let body = after.trim_end();
        s = body
            .strip_suffix("```")
            .map(str::trim)
            .unwrap_or(body)
            .trim();
    }
    let s = s
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
        .trim();
    // The model occasionally returns the account on the first line and prose
    // after — be lenient and only take line 1.
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.is_empty() || first_line.eq_ignore_ascii_case("NONE") {
        None
    } else {
        Some(first_line.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_chart() {
        let chart = Chart::parse(
            "# expenses\nExpenses:通信費\nExpenses:消耗品費\n\n# liab\nLiabilities:買掛金\n",
        )
        .unwrap();
        assert_eq!(chart.len(), 3);
        assert!(chart.contains("Expenses:通信費"));
        assert!(chart.contains("Liabilities:買掛金"));
        assert!(!chart.contains("Expenses:旅費交通費"));
    }

    #[test]
    fn strips_inline_comments_and_dedupes() {
        let chart =
            Chart::parse("Expenses:通信費  # phone & net\nExpenses:通信費\nExpenses:消耗品費\n")
                .unwrap();
        assert_eq!(chart.len(), 2);
    }

    #[test]
    fn empty_chart_errors() {
        let err = Chart::parse("# only a comment\n\n").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    /// The exact header GnuCash 4.x writes for "Export Account Tree".
    const GNUCASH_HEADER: &str = "Type,Full Account Name,Account Name,Account Code,Description,Account Color,Notes,Symbol,Namespace,Hidden,Tax Info,Placeholder";

    #[test]
    fn parses_gnucash_csv_export() {
        // Mirrors the user's actual export: ASSET parents are placeholders
        // (Placeholder=T) and the leaf is Placeholder=F. Only the leaf
        // survives; descriptions and types are captured for the prompt.
        let csv = format!(
            "{GNUCASH_HEADER}\n\
ASSET,資産,資産,,資産,,,JPY,CURRENCY,F,F,T\n\
ASSET,資産:固定資産,固定資産,,,,,JPY,CURRENCY,F,F,T\n\
ASSET,資産:固定資産:一括償却資産,一括償却資産,,,,,JPY,CURRENCY,F,F,F\n\
EXPENSE,費用:通信費,通信費,,通信に関する費用,,,JPY,CURRENCY,F,F,F\n\
EXPENSE,費用:消耗品費,消耗品費,,,,,JPY,CURRENCY,F,F,F\n"
        );
        let chart = Chart::parse(&csv).unwrap();
        assert_eq!(chart.len(), 3);
        assert!(chart.contains("資産:固定資産:一括償却資産"));
        assert!(chart.contains("費用:通信費"));
        assert!(chart.contains("費用:消耗品費"));
        // Placeholder parents are filtered out.
        assert!(!chart.contains("資産"));
        assert!(!chart.contains("資産:固定資産"));

        // Description + type surface in the rendered prompt.
        let rendered = chart.rendered();
        assert!(
            rendered.contains("[EXPENSE] 費用:通信費 — 通信に関する費用"),
            "rendered={rendered}"
        );
        assert!(rendered.contains("[EXPENSE] 費用:消耗品費\n"));
        assert!(rendered.contains("[ASSET] 資産:固定資産:一括償却資産\n"));
    }

    #[test]
    fn skips_hidden_gnucash_rows() {
        let csv = format!(
            "{GNUCASH_HEADER}\n\
EXPENSE,費用:旧勘定,旧勘定,,deprecated,,,JPY,CURRENCY,T,F,F\n\
EXPENSE,費用:通信費,通信費,,,,,JPY,CURRENCY,F,F,F\n"
        );
        let chart = Chart::parse(&csv).unwrap();
        assert_eq!(chart.len(), 1);
        assert!(!chart.contains("費用:旧勘定"));
        assert!(chart.contains("費用:通信費"));
    }

    #[test]
    fn gnucash_csv_with_only_placeholders_errors() {
        let csv = format!(
            "{GNUCASH_HEADER}\n\
ASSET,資産,資産,,,,,JPY,CURRENCY,F,F,T\n\
ASSET,資産:固定資産,固定資産,,,,,JPY,CURRENCY,F,F,T\n"
        );
        let err = Chart::parse(&csv).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("no usable accounts"), "got: {s}");
    }

    #[test]
    fn handles_utf8_bom_on_csv() {
        let csv = format!(
            "\u{feff}{GNUCASH_HEADER}\nEXPENSE,費用:通信費,通信費,,,,,JPY,CURRENCY,F,F,F\n"
        );
        let chart = Chart::parse(&csv).unwrap();
        assert!(chart.contains("費用:通信費"));
    }

    #[test]
    fn parse_pick_takes_first_line() {
        assert_eq!(
            parse_pick("Expenses:通信費").as_deref(),
            Some("Expenses:通信費")
        );
        assert_eq!(
            parse_pick("Expenses:通信費\n(rationale: ...)").as_deref(),
            Some("Expenses:通信費")
        );
    }

    #[test]
    fn parse_pick_handles_fences_and_quotes() {
        assert_eq!(
            parse_pick("```\nExpenses:通信費\n```").as_deref(),
            Some("Expenses:通信費")
        );
        assert_eq!(
            parse_pick("\"Expenses:通信費\"").as_deref(),
            Some("Expenses:通信費")
        );
    }

    #[test]
    fn parse_pick_none_sentinel() {
        assert!(parse_pick("NONE").is_none());
        assert!(parse_pick("none").is_none());
        assert!(parse_pick("").is_none());
    }
}
