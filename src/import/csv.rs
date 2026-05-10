//! `import csv` — parse a delimited file into Transaction JSONL.
//!
//! Deterministic, profile-driven. Optional `--infer` (next iteration) will
//! generate a profile by handing the first ~50 lines to an LLM; the resulting
//! TOML is saved so subsequent runs are decode-only.

use crate::import::Profile;
use crate::io::{Transaction, write_jsonl_stdout};
use anyhow::Context;
use chrono::NaiveDate;
use clap::Args;
use serde_json::json;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct CsvArgs {
    /// Input CSV path. Use `-` for stdin (UTF-8 only — encoding decoding is
    /// driven by the profile and only fires for file inputs).
    pub input: String,

    /// Profile name (looked up in `~/.config/fetchdoc/profiles/`) or path
    /// to a `.toml` file. Required; auto-inference is a separate flag.
    #[arg(long, conflicts_with = "infer")]
    pub profile: Option<String>,

    /// (Not yet implemented) Hand the first lines of the file to an LLM and
    /// generate a profile TOML. The generated profile is saved next to your
    /// other profiles and used for the run.
    #[arg(long, default_value_t = false)]
    pub infer: bool,

    /// Suppress per-row stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: CsvArgs) -> anyhow::Result<()> {
    if args.infer {
        return super::infer::run_csv(&args).await;
    }
    let profile_value = args
        .profile
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--profile is required (or use --infer)"))?;
    let profile = Profile::resolve(profile_value)
        .with_context(|| format!("loading profile {profile_value}"))?;

    let bytes = read_input_bytes(&args.input)?;
    let text = decode(&bytes, &profile.encoding)
        .with_context(|| format!("decoding input as {}", profile.encoding))?;

    parse_into_jsonl(&text, &profile, &args.input, args.quiet)
}

/// Read the whole file (or stdin) into memory. Bank statements are tiny
/// (kilobytes to a few MB) so streaming isn't worth the complexity.
fn read_input_bytes(input: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    if input == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        std::fs::read(PathBuf::from(input)).with_context(|| format!("reading {input}"))
    }
}

/// Decode raw bytes using a label like `"shift_jis"` / `"utf-8"`.
fn decode(bytes: &[u8], label: &str) -> anyhow::Result<String> {
    let enc = encoding_rs::Encoding::for_label(label.as_bytes())
        .ok_or_else(|| anyhow::anyhow!("unknown encoding label {label:?}"))?;
    let (cow, _, had_errors) = enc.decode(bytes);
    if had_errors {
        eprintln!("warning: {label} decoding had replacement chars");
    }
    Ok(cow.into_owned())
}

fn parse_into_jsonl(
    text: &str,
    profile: &Profile,
    source_label: &str,
    quiet: bool,
) -> anyhow::Result<()> {
    // Skip rows before the header. csv::ReaderBuilder doesn't have a
    // built-in skip, so we trim the leading lines manually.
    let trimmed = skip_lines(text, profile.header_row.saturating_sub(1));

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(profile.delimiter_byte()?)
        .has_headers(true)
        .flexible(true)
        .from_reader(trimmed.as_bytes());

    let headers = rdr.headers()?.clone();
    let idx = build_index(&headers, &profile.columns)
        .with_context(|| format!("profile {} header lookup", profile.name))?;

    let records = rdr
        .records()
        .skip(profile.skip_rows)
        .map(|r| r.map_err(anyhow::Error::from));
    emit_records(records, &idx, profile, "csv", source_label, quiet)
}

/// Shared row-emission loop used by both `import csv` and `import xlsx`.
/// Each input row is mapped to a [`Transaction`], written as JSONL on stdout,
/// and counted; per-row failures are skipped (with stderr warning unless
/// `quiet`) so a few bad rows don't kill an otherwise-good import.
pub(super) fn emit_records<I>(
    records: I,
    idx: &ColumnIndex,
    profile: &Profile,
    source_kind: &'static str,
    source_label: &str,
    quiet: bool,
) -> anyhow::Result<()>
where
    I: IntoIterator<Item = anyhow::Result<csv::StringRecord>>,
{
    let mut emitted = 0usize;
    let mut skipped = 0usize;
    let mut needs_review = 0usize;

    for (row_no, rec_res) in records.into_iter().enumerate() {
        let rec = match rec_res {
            Ok(r) => r,
            Err(e) => {
                skipped += 1;
                if !quiet {
                    eprintln!("skip row {}: {e:#}", row_no + 1);
                }
                continue;
            }
        };
        match build_transaction(&rec, idx, profile, source_kind, source_label) {
            Ok(tx) => {
                if tx.status == "needs_review" {
                    needs_review += 1;
                }
                write_jsonl_stdout(&tx)?;
                emitted += 1;
            }
            Err(e) => {
                skipped += 1;
                if !quiet {
                    eprintln!("skip row {}: {e:#}", row_no + 1);
                }
            }
        }
    }

    if !quiet {
        eprintln!(
            "import {source_kind}: {emitted} ok, {needs_review} needs_review, {skipped} skipped (profile {})",
            profile.name
        );
    }

    if emitted == 0 && skipped > 0 {
        anyhow::bail!("no rows could be parsed; check the profile column mapping");
    }
    Ok(())
}

/// Resolve column names → indices once, up front, so per-row lookup is O(1).
pub(super) struct ColumnIndex {
    posted_date: usize,
    description: usize,
    amount: Option<usize>,
    withdrawal: Option<usize>,
    deposit: Option<usize>,
    balance: Option<usize>,
    memo: Option<usize>,
    value_date: Option<usize>,
}

pub(super) fn build_index(
    headers: &csv::StringRecord,
    cols: &super::profile::Columns,
) -> anyhow::Result<ColumnIndex> {
    let find = |name: &str| -> anyhow::Result<usize> {
        headers
            .iter()
            .position(|h| h.trim() == name)
            .ok_or_else(|| anyhow::anyhow!("column {name:?} not found in header"))
    };
    let find_opt = |name: &Option<String>| -> anyhow::Result<Option<usize>> {
        name.as_deref().map(find).transpose()
    };

    Ok(ColumnIndex {
        posted_date: find(&cols.posted_date)?,
        description: find(&cols.description)?,
        amount: find_opt(&cols.amount)?,
        withdrawal: find_opt(&cols.withdrawal)?,
        deposit: find_opt(&cols.deposit)?,
        balance: find_opt(&cols.balance)?,
        memo: find_opt(&cols.memo)?,
        value_date: find_opt(&cols.value_date)?,
    })
}

fn build_transaction(
    rec: &csv::StringRecord,
    idx: &ColumnIndex,
    profile: &Profile,
    source_kind: &'static str,
    source_label: &str,
) -> anyhow::Result<Transaction> {
    let posted_raw = field(rec, idx.posted_date)?;
    let posted_date = parse_date(posted_raw, &profile.date_format)
        .with_context(|| format!("posted_date {posted_raw:?}"))?;

    let value_date = idx
        .value_date
        .and_then(|i| rec.get(i).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
        .map(|s| parse_date(&s, &profile.date_format))
        .transpose()?;

    let description_raw = field(rec, idx.description)?.trim().to_string();

    let amount_jpy = if let Some(i) = idx.amount {
        parse_amount(field(rec, i)?)?
    } else {
        let w = idx
            .withdrawal
            .map(|i| parse_amount_or_zero(rec.get(i).unwrap_or("")))
            .transpose()?
            .unwrap_or(0);
        let d = idx
            .deposit
            .map(|i| parse_amount_or_zero(rec.get(i).unwrap_or("")))
            .transpose()?
            .unwrap_or(0);
        // Withdrawal positive in source → outflow (negative in our schema).
        d - w
    };

    let balance_jpy = idx
        .balance
        .and_then(|i| rec.get(i).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
        .map(|s| parse_amount(&s))
        .transpose()?;

    let memo = idx
        .memo
        .and_then(|i| rec.get(i).map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let external_id = make_external_id(&profile.name, &posted_date, amount_jpy, &description_raw);

    let status = if amount_jpy == 0 {
        "needs_review"
    } else {
        "ok"
    }
    .to_string();

    Ok(Transaction {
        source: source_kind.to_string(),
        source_profile: Some(profile.name.clone()),
        external_id,
        posted_date,
        value_date,
        amount_jpy,
        balance_jpy,
        description_raw,
        description_normalized: None,
        counterparty_guess: None,
        memo,
        category_guess: None,
        source_meta: Some(json!({ "input": source_label })),
        exported: None,
        status,
    })
}

fn field(rec: &csv::StringRecord, i: usize) -> anyhow::Result<&str> {
    rec.get(i)
        .ok_or_else(|| anyhow::anyhow!("row missing column at index {i}"))
}

fn parse_date(s: &str, fmt: &str) -> anyhow::Result<String> {
    let s = s.trim();
    let d = NaiveDate::parse_from_str(s, fmt)
        .map_err(|e| anyhow::anyhow!("date {s:?} does not match {fmt:?}: {e}"))?;
    Ok(d.format("%Y-%m-%d").to_string())
}

/// Parse `"1,234"`, `"-12,100"`, `"¥1,000"`, `"1000円"`, `""` → integer JPY.
/// Empty / dash strings are an error here; use [`parse_amount_or_zero`] for
/// columns that may be blank to mean zero.
fn parse_amount(s: &str) -> anyhow::Result<i64> {
    let cleaned = clean_amount(s);
    if cleaned.is_empty() {
        anyhow::bail!("empty amount");
    }
    cleaned
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("amount {s:?} not an integer: {e}"))
}

/// Same as [`parse_amount`] but returns 0 on blank / dash columns. Used for
/// withdrawal / deposit columns, which are blank on the inactive side of each row.
fn parse_amount_or_zero(s: &str) -> anyhow::Result<i64> {
    let cleaned = clean_amount(s);
    if cleaned.is_empty() {
        return Ok(0);
    }
    cleaned
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("amount {s:?} not an integer: {e}"))
}

fn clean_amount(s: &str) -> String {
    let trimmed = s
        .trim()
        .trim_start_matches(['¥', '￥', '$'])
        .trim_end_matches(['円'])
        .replace([',', '，'], ""); // strip ASCII + full-width thousand separators
    // Some banks write a literal "-" to mean "no value"; treat that as blank.
    if trimmed.chars().all(|c| c == '-') {
        return String::new();
    }
    trimmed
}

/// Stable, version-independent id used for de-duplication on re-import.
/// FNV-1a over the load-bearing fields. Not crypto, not collision-free at
/// scale, but excellent for "did I already import this row".
fn make_external_id(profile: &str, date: &str, amount: i64, desc: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    feed(profile.as_bytes());
    feed(b"\0");
    feed(date.as_bytes());
    feed(b"\0");
    feed(amount.to_string().as_bytes());
    feed(b"\0");
    feed(desc.as_bytes());
    format!("csv:{profile}:{date}:{h:016x}")
}

/// Drop the first `n` `\n`-separated lines and return the rest as a slice.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn smbc_profile() -> Profile {
        Profile::from_toml_str(
            r#"
name = "smbc"
date_format = "%Y/%m/%d"

[columns]
posted_date = "年月日"
description = "お取り扱い内容"
withdrawal = "お支払金額"
deposit = "お預り金額"
balance = "差引残高"
"#,
        )
        .unwrap()
    }

    #[test]
    fn parse_amount_handles_yen_and_commas() {
        assert_eq!(parse_amount("1,234").unwrap(), 1234);
        assert_eq!(parse_amount(" -12,100 ").unwrap(), -12100);
        assert_eq!(parse_amount("¥1,000").unwrap(), 1000);
        assert_eq!(parse_amount("1000円").unwrap(), 1000);
        assert_eq!(parse_amount_or_zero("").unwrap(), 0);
    }

    #[test]
    fn parse_date_iso_output() {
        let d = parse_date("2026/04/30", "%Y/%m/%d").unwrap();
        assert_eq!(d, "2026-04-30");
    }

    #[test]
    fn skip_lines_works() {
        assert_eq!(skip_lines("a\nb\nc", 0), "a\nb\nc");
        assert_eq!(skip_lines("a\nb\nc", 1), "b\nc");
        assert_eq!(skip_lines("a\nb\nc", 2), "c");
    }

    #[test]
    fn external_id_is_stable_and_distinguishes_inputs() {
        let a = make_external_id("smbc", "2026-04-30", -12100, "Acme");
        let a2 = make_external_id("smbc", "2026-04-30", -12100, "Acme");
        let b = make_external_id("smbc", "2026-04-30", -12101, "Acme");
        assert_eq!(a, a2);
        assert_ne!(a, b);
    }

    #[test]
    fn csv_smbc_style_round_trip() {
        let csv = "\
年月日,お取り扱い内容,お支払金額,お預り金額,差引残高
2026/04/30,Acme,12100,,234567
2026/05/01,給与,,500000,734567
";
        let profile = smbc_profile();
        let trimmed = skip_lines(csv, 0);
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(profile.delimiter_byte().unwrap())
            .has_headers(true)
            .from_reader(trimmed.as_bytes());
        let headers = rdr.headers().unwrap().clone();
        let idx = build_index(&headers, &profile.columns).unwrap();
        let recs: Vec<_> = rdr
            .records()
            .map(|r| build_transaction(&r.unwrap(), &idx, &profile, "csv", "test.csv").unwrap())
            .collect();

        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].posted_date, "2026-04-30");
        assert_eq!(recs[0].amount_jpy, -12100);
        assert_eq!(recs[0].balance_jpy, Some(234567));
        assert_eq!(recs[0].description_raw, "Acme");
        assert_eq!(recs[1].amount_jpy, 500000);
    }
}
