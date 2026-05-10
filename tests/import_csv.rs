//! End-to-end test for `fetchdoc import csv` and `fetchdoc export gnucash`.
//!
//! Builds the binary, pipes a small SMBC-style Shift_JIS CSV through it, and
//! checks the JSONL output. Then re-pipes the JSONL into `export gnucash`
//! and checks the GnuCash CSV columns.

use assert_cmd::Command;
use std::io::Write;

fn write_sjis(path: &std::path::Path, text: &str) {
    let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(text);
    assert!(!had_errors, "input contains chars Shift_JIS cannot encode");
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&encoded).unwrap();
}

#[test]
fn smbc_csv_to_gnucash_pipeline() {
    let tmp = tempdir();
    let profile_path = tmp.join("smbc.toml");
    std::fs::write(
        &profile_path,
        r#"
name = "smbc"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "年月日"
description = "お取り扱い内容"
withdrawal  = "お支払金額"
deposit     = "お預り金額"
balance     = "差引残高"
"#,
    )
    .unwrap();

    let csv_path = tmp.join("statement.csv");
    write_sjis(
        &csv_path,
        "年月日,お取り扱い内容,お支払金額,お預り金額,差引残高\n\
         2026/04/30,アクメ,12100,,234567\n\
         2026/05/01,給与,,500000,734567\n",
    );

    // Step 1: import csv → JSONL
    let import_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "csv",
            "--profile",
            profile_path.to_str().unwrap(),
            "--quiet",
            csv_path.to_str().unwrap(),
        ])
        .output()
        .expect("run import csv");
    assert!(
        import_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&import_out.stderr)
    );

    let jsonl = String::from_utf8(import_out.stdout).unwrap();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 transaction rows: {jsonl}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["source"], "csv");
    assert_eq!(first["source_profile"], "smbc");
    assert_eq!(first["posted_date"], "2026-04-30");
    assert_eq!(first["amount_jpy"], -12100); // withdrawal → negative
    assert_eq!(first["balance_jpy"], 234567);
    assert_eq!(first["description_raw"], "アクメ");
    assert_eq!(first["status"], "ok");

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["amount_jpy"], 500000); // deposit → positive

    // Step 2: pipe JSONL → export gnucash
    let gnucash_path = tmp.join("gnucash.csv");
    let export_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            gnucash_path.to_str().unwrap(),
            "--account",
            "Assets:Bank:SMBC",
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
    assert_eq!(
        csv_lines[0],
        "Date,Description,Notes,Account,Deposit,Withdrawal,Transfer Account,Commodity/Currency"
    );
    // Withdrawal row: amount in Withdrawal column, Deposit blank.
    assert!(csv_lines[1].contains("2026-04-30"));
    assert!(csv_lines[1].contains("Assets:Bank:SMBC"));
    assert!(csv_lines[1].contains(",12100,"));
    assert!(csv_lines[1].contains("Imbalance-JPY"));
    // Deposit row: amount in Deposit column.
    assert!(csv_lines[2].contains(",500000,,"));
}

#[test]
fn halfwidth_katakana_is_normalised_into_description_normalized() {
    // Many Japanese banks emit half-width katakana in their CSV exports.
    // The importer should preserve the source bytes in description_raw and
    // populate description_normalized with the full-width form so downstream
    // tools (and humans) get readable text.
    let tmp = tempdir();
    // Different filenames from `smbc_csv_to_gnucash_pipeline` so that if the
    // nanosecond-based `tempdir()` collides under parallel test execution the
    // two tests don't clobber each other's inputs.
    let profile_path = tmp.join("smbc-halfwidth.toml");
    std::fs::write(
        &profile_path,
        r#"
name = "smbc"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "年月日"
description = "お取り扱い内容"
withdrawal  = "お支払金額"
deposit     = "お預り金額"
"#,
    )
    .unwrap();

    let csv_path = tmp.join("statement-halfwidth.csv");
    // ｱｸﾒｶﾌﾞｼｷｶﾞｲｼｬ (half-width katakana with voiced marks)
    write_sjis(
        &csv_path,
        "年月日,お取り扱い内容,お支払金額,お預り金額\n\
         2026/04/30,ｱｸﾒｶﾌﾞｼｷｶﾞｲｼｬ,12100,\n",
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "csv",
            "--profile",
            profile_path.to_str().unwrap(),
            "--quiet",
            csv_path.to_str().unwrap(),
        ])
        .output()
        .expect("run import csv");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let line = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["description_raw"], "ｱｸﾒｶﾌﾞｼｷｶﾞｲｼｬ");
    assert_eq!(v["description_normalized"], "アクメカブシキガイシャ");
}

#[test]
fn missing_profile_errors() {
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args(["import", "csv", "/nonexistent.csv"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--profile is required") || err.contains("--infer"),
        "got: {err}"
    );
}

/// Minimal scoped temp dir without pulling in `tempfile` as a dep.
fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-test-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
