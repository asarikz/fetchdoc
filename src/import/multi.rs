//! Two-file CSV join (e.g. SBI Sumishin bank statement + debit-card detail).
//!
//! The bank statement (`nyushukinmeisai_*.csv`) lists every transaction including
//! debit-card purchases as opaque `デビット ######` lines. The debit-card detail
//! CSV (`meisai_*.csv`) carries the merchant name, FX rate, foreign amount, and
//! 海外事務手数料 — but no slip number that links back to the statement.
//!
//! So we join by **(date ± window, total amount)** where:
//!   - statement `withdrawal == debit.tx_amount + tx_fee + atm_fee + fx_fee`
//!   - the debit slip date and the bank-posted date may differ by a few days
//!     (settlement lag), which `multi.join.date_window_days` covers.
//!
//! For each matched row we:
//!   - set `counterparty_guess` = merchant
//!   - emit `splits` with the principal and any non-zero fees on separate
//!     GnuCash accounts (so 海外事務手数料 ends up in `Expenses:支払手数料:…`)
//!   - record FX info in `source_meta`
//!
//! Non-matching primary rows (transfers, salaries, ATM withdrawals not via debit)
//! pass through as plain single-split transactions, identical to the
//! `import csv <single-file>` path.

use crate::import::csv::{build_index, build_transaction_public, decode};
use crate::import::profile::{DebitColumns, DebitFile, MultiConfig, Profile, RowFilter};
use crate::io::{Split, Transaction, write_jsonl_stdout};
use anyhow::Context;
use chrono::NaiveDate;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Entry point from `import csv --dir <path>`.
pub(super) fn run(profile: &Profile, dir: &Path, quiet: bool) -> anyhow::Result<()> {
    let multi = profile
        .multi
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--dir requires a profile with a [multi] section"))?;

    let primary_paths = glob_in(dir, &multi.primary_glob)?;
    let debit_paths = glob_in(dir, &multi.debit.glob)?;
    if primary_paths.is_empty() {
        anyhow::bail!(
            "no files matching {:?} in {}",
            multi.primary_glob,
            dir.display()
        );
    }

    let primary_match = multi
        .primary_match_regex
        .as_deref()
        .map(|p| Regex::new(p).with_context(|| format!("compiling primary_match_regex {p:?}")))
        .transpose()?;

    let debit_index = build_debit_index(&debit_paths, multi, quiet)?;
    let mut consumed = vec![false; debit_index.entries.len()];

    let mut emitted = 0usize;
    let mut needs_review = 0usize;
    let mut joined = 0usize;
    let mut skipped = 0usize;

    for path in &primary_paths {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let text = decode(&bytes, &profile.encoding)
            .with_context(|| format!("decoding {} as {}", path.display(), profile.encoding))?;

        let trimmed = skip_lines(&text, profile.header_row.saturating_sub(1));
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(profile.delimiter_byte()?)
            .has_headers(true)
            .flexible(true)
            .from_reader(trimmed.as_bytes());

        let headers = rdr.headers()?.clone();
        let idx = build_index(&headers, &profile.columns)
            .with_context(|| format!("profile {} header lookup", profile.name))?;

        for (row_no, rec_res) in rdr.records().skip(profile.skip_rows).enumerate() {
            let rec = match rec_res {
                Ok(r) => r,
                Err(e) => {
                    skipped += 1;
                    if !quiet {
                        eprintln!("skip {} row {}: {e:#}", path.display(), row_no + 1);
                    }
                    continue;
                }
            };
            // Build the base Transaction using the existing single-file path.
            let mut tx = match build_transaction_public(
                &rec,
                &idx,
                profile,
                path.to_string_lossy().as_ref(),
            ) {
                Ok(t) => t,
                Err(e) => {
                    skipped += 1;
                    if !quiet {
                        eprintln!("skip {} row {}: {e:#}", path.display(), row_no + 1);
                    }
                    continue;
                }
            };

            // Decide whether this row should attempt a debit join.
            let matches_debit = primary_match
                .as_ref()
                .is_none_or(|re| re.is_match(&tx.description_raw));

            if matches_debit && tx.amount_jpy < 0 {
                let want_total = -tx.amount_jpy;
                if let Some(hit) =
                    debit_index.find(&tx.posted_date, want_total, multi.join.date_window_days)
                {
                    if consumed[hit.index] {
                        // Already used by an earlier primary row; flag for review.
                        tx.status = "needs_review".into();
                        needs_review += 1;
                        if !quiet {
                            eprintln!(
                                "warn {}: debit row already consumed (date={}, total={}); marking needs_review",
                                path.display(),
                                tx.posted_date,
                                want_total
                            );
                        }
                    } else {
                        consumed[hit.index] = true;
                        apply_debit(&mut tx, &debit_index.entries[hit.index], multi);
                        joined += 1;
                    }
                } else {
                    // No detail row found within the window — keep the row as-is
                    // but flag it so the user can fill in the merchant by hand.
                    tx.status = "needs_review".into();
                    needs_review += 1;
                    if !quiet {
                        eprintln!(
                            "warn {}: no debit detail for {} ({} JPY); needs_review",
                            path.display(),
                            tx.posted_date,
                            want_total
                        );
                    }
                }
            }

            write_jsonl_stdout(&tx)?;
            emitted += 1;
        }
    }

    // Report any debit-detail rows that never matched a statement row. These
    // are the most useful diagnostic when the join window is too tight or a
    // statement file is missing.
    let unmatched: Vec<_> = consumed
        .iter()
        .enumerate()
        .filter(|(_, c)| !**c)
        .map(|(i, _)| &debit_index.entries[i])
        .collect();
    if !unmatched.is_empty() && !quiet {
        eprintln!(
            "import csv: {} debit detail row(s) had no matching statement entry:",
            unmatched.len()
        );
        for d in &unmatched {
            eprintln!("  {} {} JPY ({})", d.date, d.total, d.merchant);
        }
    }

    if !quiet {
        eprintln!(
            "import csv: {emitted} ok, {joined} joined, {needs_review} needs_review, {skipped} skipped (profile {}, dir {})",
            profile.name,
            dir.display(),
        );
    }

    if emitted == 0 {
        anyhow::bail!("no rows could be parsed; check the profile column mapping");
    }
    Ok(())
}

/// Apply a matched debit-detail entry to a primary `Transaction`: set the
/// counterparty guess, build splits for principal + non-zero fees, and stash
/// FX info in `source_meta`.
fn apply_debit(tx: &mut Transaction, d: &DebitEntry, multi: &MultiConfig) {
    tx.counterparty_guess = Some(d.merchant.clone());

    // Build splits only when we actually have fees to separate. For a plain
    // domestic debit (no fees) the row is identical in shape to a single-leg
    // statement entry, so we leave `splits = None` and let the GnuCash export
    // use the legacy single-row form. This keeps the JSONL minimal and avoids
    // a no-op multi-split CSV that would just confuse the user at import time.
    let mut fee_splits: Vec<Split> = Vec::new();
    if let Some(v) = d.fx_fee {
        if v != 0 {
            fee_splits.push(Split {
                account: multi.join.fx_fee_account.clone(),
                amount_jpy: v,
                note: Some("海外事務手数料".into()),
            });
        }
    }
    if let Some(v) = d.tx_fee {
        if v != 0 {
            fee_splits.push(Split {
                account: multi.join.tx_fee_account.clone(),
                amount_jpy: v,
                note: Some("お取引手数料".into()),
            });
        }
    }
    if let Some(v) = d.atm_fee {
        if v != 0 {
            fee_splits.push(Split {
                account: multi.join.atm_fee_account.clone(),
                amount_jpy: v,
                note: Some("ATM手数料".into()),
            });
        }
    }
    if !fee_splits.is_empty() {
        // Principal first, then each fee. Principal account is left to
        // `category_guess` / `--default-other` at export time.
        let mut splits = Vec::with_capacity(fee_splits.len() + 1);
        splits.push(Split {
            account: None,
            amount_jpy: d.tx_amount,
            note: None,
        });
        splits.extend(fee_splits);
        tx.splits = Some(splits);
    }

    let mut meta = match tx.source_meta.take() {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    meta.insert(
        "debit".into(),
        json!({
            "merchant": d.merchant,
            "date": d.date,
            "tx_amount": d.tx_amount,
            "tx_fee": d.tx_fee,
            "atm_fee": d.atm_fee,
            "fx_fee": d.fx_fee,
            "use_currency": d.use_currency,
            "use_amount": d.use_amount,
            "rate": d.rate,
            "input": d.input,
        }),
    );
    tx.source_meta = Some(serde_json::Value::Object(meta));
}

/// One row of the debit-detail CSV after parsing.
#[derive(Debug, Clone)]
struct DebitEntry {
    date: String, // YYYY-MM-DD
    merchant: String,
    /// JPY total (= settled-side principal). Sums with fees produce the bank-
    /// statement withdrawal.
    tx_amount: i64,
    tx_fee: Option<i64>,
    atm_fee: Option<i64>,
    fx_fee: Option<i64>,
    /// Sum used for join: tx_amount + all non-None fees.
    total: i64,
    use_currency: Option<String>,
    use_amount: Option<f64>,
    rate: Option<f64>,
    input: String,
}

struct DebitIndex {
    entries: Vec<DebitEntry>,
    /// (date, total) → list of indices into `entries`. Lookup walks the slice
    /// across the date window.
    by_total: HashMap<i64, Vec<usize>>,
}

impl DebitIndex {
    fn find(&self, primary_date: &str, total: i64, window_days: u32) -> Option<DebitHit> {
        let primary_naive = NaiveDate::parse_from_str(primary_date, "%Y-%m-%d").ok()?;
        let candidates = self.by_total.get(&total)?;
        let mut best: Option<(i64, usize)> = None;
        for &i in candidates {
            let d = NaiveDate::parse_from_str(&self.entries[i].date, "%Y-%m-%d").ok()?;
            let delta = (primary_naive - d).num_days().abs();
            if delta <= window_days as i64 && best.map(|(b, _)| delta < b).unwrap_or(true) {
                best = Some((delta, i));
            }
        }
        best.map(|(_, i)| DebitHit { index: i })
    }
}

struct DebitHit {
    index: usize,
}

fn build_debit_index(
    paths: &[PathBuf],
    multi: &MultiConfig,
    quiet: bool,
) -> anyhow::Result<DebitIndex> {
    let mut entries: Vec<DebitEntry> = Vec::new();
    for path in paths {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let text = decode(&bytes, &multi.debit.encoding)
            .with_context(|| format!("decoding {} as {}", path.display(), multi.debit.encoding))?;
        parse_debit_file(&text, &multi.debit, path, &mut entries, quiet)?;
    }

    let mut by_total: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        by_total.entry(e.total).or_default().push(i);
    }
    Ok(DebitIndex { entries, by_total })
}

fn parse_debit_file(
    text: &str,
    cfg: &DebitFile,
    path: &Path,
    sink: &mut Vec<DebitEntry>,
    quiet: bool,
) -> anyhow::Result<()> {
    let trimmed = skip_lines(text, cfg.header_row.saturating_sub(1));
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(cfg.delimiter_byte()?)
        .has_headers(true)
        .flexible(true)
        .from_reader(trimmed.as_bytes());

    let headers = rdr.headers()?.clone();
    let idx = DebitIdx::resolve(&headers, &cfg.columns, cfg.row_filter.as_ref())
        .with_context(|| format!("debit header lookup in {}", path.display()))?;

    for (row_no, rec_res) in rdr.records().skip(cfg.skip_rows).enumerate() {
        let rec = match rec_res {
            Ok(r) => r,
            Err(e) => {
                if !quiet {
                    eprintln!("skip debit {} row {}: {e:#}", path.display(), row_no + 1);
                }
                continue;
            }
        };
        if let Some((col_i, want)) = &idx.row_filter {
            let cell = rec.get(*col_i).unwrap_or("").trim();
            if cell != want {
                continue;
            }
        }

        let entry = match build_debit_entry(&rec, &idx, cfg, path) {
            Ok(e) => e,
            Err(e) => {
                if !quiet {
                    eprintln!("skip debit {} row {}: {e:#}", path.display(), row_no + 1);
                }
                continue;
            }
        };
        sink.push(entry);
    }
    Ok(())
}

struct DebitIdx {
    date: usize,
    merchant: usize,
    tx_amount: usize,
    tx_fee: Option<usize>,
    atm_fee: Option<usize>,
    fx_fee: Option<usize>,
    use_currency: Option<usize>,
    use_amount: Option<usize>,
    rate: Option<usize>,
    /// (column index, expected cell value) from `multi.debit.row_filter`.
    row_filter: Option<(usize, String)>,
}

impl DebitIdx {
    fn resolve(
        headers: &csv::StringRecord,
        cols: &DebitColumns,
        row_filter: Option<&RowFilter>,
    ) -> anyhow::Result<Self> {
        let find = |name: &str| -> anyhow::Result<usize> {
            headers
                .iter()
                .position(|h| h.trim() == name)
                .ok_or_else(|| anyhow::anyhow!("debit column {name:?} not found in header"))
        };
        let find_opt = |name: &Option<String>| -> anyhow::Result<Option<usize>> {
            name.as_deref().map(find).transpose()
        };
        let row_filter = row_filter
            .map(|f| -> anyhow::Result<(usize, String)> { Ok((find(&f.column)?, f.value.clone())) })
            .transpose()?;
        Ok(Self {
            date: find(&cols.date)?,
            merchant: find(&cols.merchant)?,
            tx_amount: find(&cols.tx_amount)?,
            tx_fee: find_opt(&cols.tx_fee)?,
            atm_fee: find_opt(&cols.atm_fee)?,
            fx_fee: find_opt(&cols.fx_fee)?,
            use_currency: find_opt(&cols.use_currency)?,
            use_amount: find_opt(&cols.use_amount)?,
            rate: find_opt(&cols.rate)?,
            row_filter,
        })
    }
}

fn build_debit_entry(
    rec: &csv::StringRecord,
    idx: &DebitIdx,
    cfg: &DebitFile,
    path: &Path,
) -> anyhow::Result<DebitEntry> {
    let date_raw = rec
        .get(idx.date)
        .ok_or_else(|| anyhow::anyhow!("missing date column"))?;
    let date = NaiveDate::parse_from_str(date_raw.trim(), &cfg.date_format)
        .map_err(|e| anyhow::anyhow!("date {date_raw:?} vs {:?}: {e}", cfg.date_format))?
        .format("%Y-%m-%d")
        .to_string();

    let merchant = rec
        .get(idx.merchant)
        .ok_or_else(|| anyhow::anyhow!("missing merchant column"))?
        .trim()
        .to_string();

    let tx_amount = parse_decimal_jpy(
        rec.get(idx.tx_amount)
            .ok_or_else(|| anyhow::anyhow!("missing tx_amount"))?,
    )?;
    let tx_fee = idx
        .tx_fee
        .and_then(|i| rec.get(i).map(parse_decimal_jpy).transpose().ok().flatten());
    let atm_fee = idx
        .atm_fee
        .and_then(|i| rec.get(i).map(parse_decimal_jpy).transpose().ok().flatten());
    let fx_fee = idx
        .fx_fee
        .and_then(|i| rec.get(i).map(parse_decimal_jpy).transpose().ok().flatten());

    let total = tx_amount + tx_fee.unwrap_or(0) + atm_fee.unwrap_or(0) + fx_fee.unwrap_or(0);

    let use_currency = idx
        .use_currency
        .and_then(|i| rec.get(i))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let use_amount = idx
        .use_amount
        .and_then(|i| rec.get(i))
        .and_then(parse_decimal_f64);
    let rate = idx
        .rate
        .and_then(|i| rec.get(i))
        .and_then(parse_decimal_f64);

    Ok(DebitEntry {
        date,
        merchant,
        tx_amount,
        tx_fee,
        atm_fee,
        fx_fee,
        total,
        use_currency,
        use_amount,
        rate,
        input: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
    })
}

/// Parse SBI's `"2452.00"` / `"61.00"` decimal-but-integer JPY values. Empty
/// or dash-only cells are an error here; callers wrap in `Option`.
fn parse_decimal_jpy(s: &str) -> anyhow::Result<i64> {
    let trimmed = s.trim().trim_end_matches('円').replace(',', "");
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '-') {
        anyhow::bail!("empty");
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return Ok(n);
    }
    let f: f64 = trimmed
        .parse()
        .map_err(|e| anyhow::anyhow!("amount {s:?}: {e}"))?;
    if (f.fract()).abs() > 0.5 {
        // SBI debit CSVs are JPY-integer with `.00` cosmetics. Anything with
        // a real fractional part is a parser bug or an unexpected schema.
        anyhow::bail!("amount {s:?} has a non-integer JPY value");
    }
    Ok(f.round() as i64)
}

fn parse_decimal_f64(s: &str) -> Option<f64> {
    let t = s.trim().replace(',', "");
    if t.is_empty() {
        return None;
    }
    t.parse().ok()
}

fn skip_lines(text: &str, n: usize) -> &str {
    if n == 0 {
        return text;
    }
    let mut remaining = n;
    let mut idx = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            remaining -= 1;
            if remaining == 0 {
                idx = i + 1;
                break;
            }
        }
    }
    &text[idx..]
}

/// Glob-like file matching scoped to `dir`. We deliberately don't pull in the
/// `glob` crate — the patterns we accept (`prefix_*.csv`) only need a literal
/// prefix and a trailing extension, and rolling our own avoids a dependency.
fn glob_in(dir: &Path, pattern: &str) -> anyhow::Result<Vec<PathBuf>> {
    let (prefix, suffix) = pattern
        .split_once('*')
        .ok_or_else(|| anyhow::anyhow!("glob {pattern:?} must contain a single '*'"))?;
    if suffix.contains('*') || prefix.contains('*') {
        anyhow::bail!("glob {pattern:?}: only one '*' wildcard supported");
    }
    let mut out = Vec::new();
    let read = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DebitFile {
        DebitFile {
            glob: "meisai_*.csv".into(),
            encoding: "utf-8".into(),
            delimiter: ",".into(),
            header_row: 1,
            skip_rows: 0,
            date_format: "%Y/%m/%d".into(),
            row_filter: Some(RowFilter {
                column: "1".into(),
                value: "2".into(),
            }),
            columns: DebitColumns {
                date: "お取引日".into(),
                merchant: "お取引内容".into(),
                tx_amount: "お取引金額".into(),
                tx_fee: Some("お取引手数料".into()),
                atm_fee: Some("ATM手数料".into()),
                fx_fee: Some("海外事務手数料".into()),
                use_currency: Some("ご利用通貨".into()),
                use_amount: Some("ご利用金額".into()),
                rate: Some("換算レート".into()),
            },
        }
    }

    fn sbi_debit_csv() -> &'static str {
        "1,お取引日,お取引内容,お取引通貨,お取引金額,お取引手数料,ATM手数料,海外事務手数料,ご利用通貨,ご利用金額,ご利用手数料,換算レート\n2,2026/05/09,CLOUDFLARE,JPY,2452.00,0.00,0.00,61.00,USD,15.62,0.00,156.97\n2,2026/05/01,GOOGLE*WORKSPACE ARTHU,JPY,2090.00,0.00,0.00,0.00,,0.00,0.00,0.00\n"
    }

    #[test]
    fn parses_sbi_debit_csv_with_row_marker() {
        let mut sink = Vec::new();
        parse_debit_file(
            sbi_debit_csv(),
            &cfg(),
            Path::new("meisai_test.csv"),
            &mut sink,
            true,
        )
        .unwrap();
        assert_eq!(sink.len(), 2);
        let foreign = sink.iter().find(|e| e.merchant == "CLOUDFLARE").unwrap();
        assert_eq!(foreign.date, "2026-05-09");
        assert_eq!(foreign.tx_amount, 2452);
        assert_eq!(foreign.fx_fee, Some(61));
        assert_eq!(foreign.total, 2513);
        assert_eq!(foreign.use_currency.as_deref(), Some("USD"));
        assert!(foreign.use_amount.unwrap() > 15.0);

        let domestic = sink
            .iter()
            .find(|e| e.merchant.starts_with("GOOGLE"))
            .unwrap();
        assert_eq!(domestic.fx_fee, Some(0));
        assert_eq!(domestic.total, 2090);
    }

    #[test]
    fn row_filter_skips_non_data_rows() {
        // Inject a stray row whose marker is "9" (e.g. a trailer); it must
        // be dropped without aborting.
        let csv = "1,お取引日,お取引内容,お取引通貨,お取引金額,お取引手数料,ATM手数料,海外事務手数料,ご利用通貨,ご利用金額,ご利用手数料,換算レート\n2,2026/05/01,Acme,JPY,1000.00,0.00,0.00,0.00,,0.00,0.00,0.00\n9,trailer,,,,,,,,,,\n";
        let mut sink = Vec::new();
        parse_debit_file(csv, &cfg(), Path::new("x.csv"), &mut sink, true).unwrap();
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].merchant, "Acme");
    }

    #[test]
    fn debit_index_finds_within_window() {
        let entries = vec![DebitEntry {
            date: "2026-05-09".into(),
            merchant: "CLOUDFLARE".into(),
            tx_amount: 2452,
            tx_fee: Some(0),
            atm_fee: Some(0),
            fx_fee: Some(61),
            total: 2513,
            use_currency: Some("USD".into()),
            use_amount: Some(15.62),
            rate: Some(156.97),
            input: "meisai.csv".into(),
        }];
        let mut by_total: HashMap<i64, Vec<usize>> = HashMap::new();
        by_total.insert(2513, vec![0]);
        let idx = DebitIndex { entries, by_total };

        // Same day → match.
        assert!(idx.find("2026-05-09", 2513, 7).is_some());
        // 3 days earlier on the statement → still within window.
        assert!(idx.find("2026-05-12", 2513, 7).is_some());
        // 10 days earlier → outside.
        assert!(idx.find("2026-05-19", 2513, 7).is_none());
        // Wrong amount → no match.
        assert!(idx.find("2026-05-09", 2500, 7).is_none());
    }

    #[test]
    fn parse_decimal_jpy_handles_sbi_format() {
        assert_eq!(parse_decimal_jpy("2452.00").unwrap(), 2452);
        assert_eq!(parse_decimal_jpy(" 61.00 ").unwrap(), 61);
        assert_eq!(parse_decimal_jpy("1,234").unwrap(), 1234);
        assert!(parse_decimal_jpy("").is_err());
        assert!(parse_decimal_jpy("-").is_err());
    }
}
