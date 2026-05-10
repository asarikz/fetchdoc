//! End-to-end test for `fetchdoc import csv --dir` against a synthetic
//! 住信SBIネット銀行 layout: bank statement (`nyushukinmeisai_*.csv`) joined
//! with the debit-card detail (`meisai_*.csv`).
//!
//! Verifies:
//! - the join finds debit detail rows even when the bank-posted date lags the
//!   slip date by a few days,
//! - foreign rows produce multi-split JSONL (`splits`) with 海外事務手数料 on
//!   its own GnuCash account,
//! - domestic debit rows (no FX fee) emit no splits,
//! - non-debit rows (e.g. salary deposit) pass through as plain transactions,
//! - the GnuCash export of the foreign row uses the multi-row continuation
//!   format (Date empty on rows 2..N).

use assert_cmd::Command;
use std::io::Write;

fn write_sjis(path: &std::path::Path, text: &str) {
    let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(text);
    assert!(!had_errors, "input contains chars Shift_JIS cannot encode");
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&encoded).unwrap();
}

/// Per-test temp dir. Includes the test name so the two tests in this file
/// can't collide when run in parallel (they also include nanos for uniqueness
/// across runs on slow filesystems).
fn tempdir(test: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-sbi-{test}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn sbi_two_csv_join_with_fx_fee_split() {
    let tmp = tempdir("join-fx");

    let profile_path = tmp.join("sbi.toml");
    std::fs::write(
        &profile_path,
        r#"
name = "sbi-sumishin"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "日付"
description = "内容"
withdrawal  = "出金金額(円)"
deposit     = "入金金額(円)"
balance     = "残高(円)"
memo        = "メモ"

[multi]
primary_glob       = "nyushukinmeisai_*.csv"
primary_match_regex = '^デビット'

[multi.debit]
glob        = "meisai_*.csv"
encoding    = "shift_jis"
date_format = "%Y/%m/%d"

[multi.debit.row_filter]
column = "1"
value  = "2"

[multi.debit.columns]
date         = "お取引日"
merchant     = "お取引内容"
tx_amount    = "お取引金額"
tx_fee       = "お取引手数料"
atm_fee      = "ATM手数料"
fx_fee       = "海外事務手数料"
use_currency = "ご利用通貨"
use_amount   = "ご利用金額"
rate         = "換算レート"

[multi.join]
date_window_days = 7
fx_fee_account   = "Expenses:支払手数料:海外事務手数料"
"#,
    )
    .unwrap();

    // Statement: 5/12 debit (matches CLOUDFLARE on 5/9 — 3-day settlement lag),
    // 5/01 debit (same-day match), and a salary deposit on 5/15 that should
    // pass through as a plain (single-split) transaction.
    let statement_path = tmp.join("nyushukinmeisai_20260520.csv");
    write_sjis(
        &statement_path,
        "日付,内容,出金金額(円),入金金額(円),残高(円),メモ\n\
         2026/05/01,デビット　９４３６５９,2090,,8904269,-\n\
         2026/05/12,デビット　９４９９８３,2513,,8901756,-\n\
         2026/05/15,給与,,500000,9401756,-\n",
    );

    // Debit detail: CLOUDFLARE on 5/09 (foreign, 61 JPY FX fee), GOOGLE on
    // 5/01 (domestic, no fee). Note the leading "1"/"2" marker column.
    let debit_path = tmp.join("meisai_20260520105656787.csv");
    write_sjis(
        &debit_path,
        "1,お取引日,お取引内容,お取引通貨,お取引金額,お取引手数料,ATM手数料,海外事務手数料,ご利用通貨,ご利用金額,ご利用手数料,換算レート\n\
         2,2026/05/09,CLOUDFLARE,JPY,2452.00,0.00,0.00,61.00,USD,15.62,0.00,156.97\n\
         2,2026/05/01,GOOGLE*WORKSPACE ARTHU,JPY,2090.00,0.00,0.00,0.00,,0.00,0.00,0.00\n",
    );

    let import_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "csv",
            "--profile",
            profile_path.to_str().unwrap(),
            "--dir",
            tmp.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run import csv --dir");
    assert!(
        import_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&import_out.stderr)
    );

    let jsonl = String::from_utf8(import_out.stdout).unwrap();
    let rows: Vec<serde_json::Value> = jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 3, "expected 3 transactions: {jsonl}");

    // Order matches the statement (filename + row order). Index 0 = 5/01 GOOGLE.
    let google = &rows[0];
    assert_eq!(google["amount_jpy"], -2090);
    assert_eq!(google["counterparty_guess"], "GOOGLE*WORKSPACE ARTHU");
    assert!(
        google.get("splits").is_none_or(|v| v.is_null()),
        "domestic debit should not emit splits: {google}"
    );

    // Index 1 = 5/12 CLOUDFLARE (foreign): debit slip is 5/09, statement post
    // is 5/12 — within the 7-day window.
    let cloud = &rows[1];
    assert_eq!(cloud["amount_jpy"], -2513);
    assert_eq!(cloud["counterparty_guess"], "CLOUDFLARE");
    let splits = cloud["splits"].as_array().expect("splits present");
    assert_eq!(splits.len(), 2, "principal + FX fee: {cloud}");
    assert_eq!(splits[0]["amount_jpy"], 2452);
    assert!(splits[0].get("account").is_none_or(|v| v.is_null()));
    assert_eq!(splits[1]["amount_jpy"], 61);
    assert_eq!(splits[1]["account"], "Expenses:支払手数料:海外事務手数料");
    let meta = &cloud["source_meta"]["debit"];
    assert_eq!(meta["use_currency"], "USD");
    assert!(meta["rate"].as_f64().unwrap() > 150.0);

    // Index 2 = salary deposit; pass-through, no debit join.
    let salary = &rows[2];
    assert_eq!(salary["amount_jpy"], 500000);
    assert!(salary.get("counterparty_guess").is_none_or(|v| v.is_null()));
    assert!(salary.get("splits").is_none_or(|v| v.is_null()));

    // Step 2: export gnucash. The CLOUDFLARE row should produce 3 CSV rows:
    // bank leg + 2 split continuations (Date empty on rows 2 & 3).
    let gnucash_path = tmp.join("gnucash.csv");
    let export_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            gnucash_path.to_str().unwrap(),
            "--account",
            "Assets:Bank:SBI",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export gnucash");
    assert!(
        export_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&export_out.stderr)
    );

    let csv_text = std::fs::read_to_string(&gnucash_path).unwrap();
    let csv_lines: Vec<&str> = csv_text.lines().collect();
    // Header + 1 (GOOGLE single) + 3 (CLOUDFLARE multi) + 1 (salary single) = 6
    assert_eq!(csv_lines.len(), 6, "expected 6 CSV lines, got:\n{csv_text}");

    // CLOUDFLARE bank leg (line 3): Date present, Withdrawal 2513.
    assert!(csv_lines[2].contains("2026-05-12"));
    assert!(csv_lines[2].contains("CLOUDFLARE"));
    assert!(csv_lines[2].contains(",2513,"));

    // First continuation: empty Date, Deposit=2452.
    assert!(csv_lines[3].starts_with(","));
    assert!(csv_lines[3].contains(",2452,"));

    // FX fee continuation: empty Date, Deposit=61, FX expense account.
    assert!(csv_lines[4].starts_with(","));
    assert!(csv_lines[4].contains(",61,"));
    assert!(csv_lines[4].contains("Expenses:支払手数料:海外事務手数料"));
}

#[test]
fn sbi_unmatched_debit_marks_needs_review() {
    let tmp = tempdir("unmatched");
    let profile_path = tmp.join("sbi.toml");
    std::fs::write(
        &profile_path,
        r#"
name = "sbi-sumishin"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "日付"
description = "内容"
withdrawal  = "出金金額(円)"
deposit     = "入金金額(円)"
balance     = "残高(円)"

[multi]
primary_glob        = "nyushukinmeisai_*.csv"
primary_match_regex = '^デビット'

[multi.debit]
glob        = "meisai_*.csv"
encoding    = "shift_jis"
date_format = "%Y/%m/%d"

[multi.debit.row_filter]
column = "1"
value  = "2"

[multi.debit.columns]
date      = "お取引日"
merchant  = "お取引内容"
tx_amount = "お取引金額"
fx_fee    = "海外事務手数料"

[multi.join]
date_window_days = 2
"#,
    )
    .unwrap();

    let statement_path = tmp.join("nyushukinmeisai_20260520.csv");
    // Statement on 5/20 — debit slip on 5/09 (11 days off) is outside the
    // 2-day window, so the join must fail and the row must be flagged.
    write_sjis(
        &statement_path,
        "日付,内容,出金金額(円),入金金額(円),残高(円)\n\
         2026/05/20,デビット　９９９９９９,2513,,8900000\n",
    );
    let debit_path = tmp.join("meisai_20260520.csv");
    write_sjis(
        &debit_path,
        "1,お取引日,お取引内容,お取引通貨,お取引金額,お取引手数料,ATM手数料,海外事務手数料,ご利用通貨,ご利用金額,ご利用手数料,換算レート\n\
         2,2026/05/09,CLOUDFLARE,JPY,2452.00,0.00,0.00,61.00,USD,15.62,0.00,156.97\n",
    );

    let import_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "csv",
            "--profile",
            profile_path.to_str().unwrap(),
            "--dir",
            tmp.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run");
    assert!(import_out.status.success());

    let jsonl = String::from_utf8(import_out.stdout).unwrap();
    let row: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(row["status"], "needs_review");
    assert!(row.get("splits").is_none_or(|v| v.is_null()));
}
