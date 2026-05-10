//! End-to-end test for `fetchdoc export gnucash` on Document JSONL.
//!
//! Pipes a small invoice stream through the binary and checks the resulting
//! GnuCash CSV uses the standard accrual A/P entry: debit Expenses, credit
//! Liabilities. Also verifies a Document missing `extracted` is downgraded
//! to `needs_review` rather than aborting the run.

use assert_cmd::Command;

#[test]
fn invoice_becomes_debit_expense_credit_payable() {
    let tmp = tempdir();
    let csv_path = tmp.join("ap.csv");

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::json!({
            "source": "gmail",
            "external_id": "msg-1",
            "attachment_path": "/cache/a.pdf",
            "extracted": {
                "transaction_date": "2026-04-30",
                "total_amount_jpy": 12100,
                "counterparty_name": "アクメ商事",
                "counterparty_t_number": "T1234567890123",
                "confidence": 0.94
            },
            "status": "ok"
        }),
        serde_json::json!({
            "source": "gmail",
            "external_id": "msg-2",
            "attachment_path": "/cache/b.pdf",
            "extracted": {
                "transaction_date": "2026-05-01",
                "total_amount_jpy": 3300,
                "counterparty_name": "B社",
                "confidence": 0.9
            },
            "status": "ok"
        })
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            csv_path.to_str().unwrap(),
            "--debit-account",
            "Expenses:諸経費",
            "--credit-account",
            "Liabilities:買掛金",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export gnucash");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let csv_text = std::fs::read_to_string(&csv_path).unwrap();
    let lines: Vec<&str> = csv_text.lines().collect();
    assert_eq!(
        lines[0],
        "Date,Description,Notes,Account,Deposit,Withdrawal,Transfer Account,Commodity/Currency"
    );
    // Row 1: アクメ. T number lands in Notes.
    assert!(lines[1].contains("2026-04-30"));
    assert!(lines[1].contains("アクメ商事"));
    assert!(lines[1].contains("T1234567890123"));
    assert!(lines[1].contains("Expenses:諸経費"));
    assert!(lines[1].contains(",12100,,")); // Deposit=12100, Withdrawal blank
    assert!(lines[1].contains("Liabilities:買掛金"));
    assert!(lines[1].ends_with("JPY"));
    // Row 2: B社, no T number → Notes is just external_id.
    assert!(lines[2].contains("2026-05-01"));
    assert!(lines[2].contains("B社"));
    assert!(lines[2].contains(",3300,,"));

    // JSONL passthrough has exported.gnucash.out
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        first["exported"]["gnucash"]["out"],
        csv_path.to_str().unwrap()
    );
    assert_eq!(first["status"], "ok");
}

#[test]
fn document_without_extracted_is_skipped_as_needs_review() {
    let tmp = tempdir();
    let csv_path = tmp.join("ap.csv");

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::json!({
            "source": "gmail",
            "external_id": "no-classify",
            "attachment_path": "/cache/x.pdf",
            "status": "ok"
        }),
        serde_json::json!({
            "source": "gmail",
            "external_id": "ok",
            "attachment_path": "/cache/y.pdf",
            "extracted": {
                "transaction_date": "2026-04-30",
                "total_amount_jpy": 100,
                "counterparty_name": "X",
                "confidence": 1.0
            },
            "status": "ok"
        })
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            csv_path.to_str().unwrap(),
            "--debit-account",
            "Expenses:諸経費",
            "--credit-account",
            "Liabilities:買掛金",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export gnucash");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let csv_text = std::fs::read_to_string(&csv_path).unwrap();
    let lines: Vec<&str> = csv_text.lines().collect();
    // Header + only the second (ok) row.
    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains("X"));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let jl: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(jl.len(), 2);
    assert_eq!(jl[0]["status"], "needs_review");
    assert!(jl[0].get("exported").is_none());
    assert_eq!(jl[1]["status"], "ok");
    assert!(jl[1]["exported"]["gnucash"]["out"].is_string());
}

#[test]
fn document_input_requires_credit_account() {
    let tmp = tempdir();
    let csv_path = tmp.join("ap.csv");
    let jsonl = serde_json::json!({
        "source": "gmail",
        "external_id": "x",
        "attachment_path": "/p.pdf",
        "extracted": {
            "transaction_date": "2026-04-30",
            "total_amount_jpy": 1,
            "counterparty_name": "X",
            "confidence": 1.0
        },
        "status": "ok"
    })
    .to_string()
        + "\n";

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            csv_path.to_str().unwrap(),
            "--debit-account",
            "Expenses:X",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--credit-account"), "got: {err}");
}

#[test]
fn picker_mode_rejects_empty_chart_before_writing_csv() {
    let tmp = tempdir();
    let csv_path = tmp.join("ap.csv");
    let chart_path = tmp.join("accounts.txt");
    std::fs::write(&chart_path, "# nothing but a comment\n\n").unwrap();

    let jsonl = serde_json::json!({
        "source": "gmail",
        "external_id": "x",
        "attachment_path": "/p.pdf",
        "extracted": {
            "transaction_date": "2026-04-30",
            "total_amount_jpy": 1,
            "counterparty_name": "X",
            "confidence": 1.0
        },
        "status": "ok"
    })
    .to_string()
        + "\n";

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "gnucash",
            "--out",
            csv_path.to_str().unwrap(),
            "--accounts",
            chart_path.to_str().unwrap(),
            "--credit-account",
            "Liabilities:買掛金",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("empty"), "got: {err}");
    // CSV must NOT have been opened/truncated when init fails.
    assert!(
        !csv_path.exists(),
        "csv file should not be created on init error"
    );
}

#[test]
fn picker_mode_without_api_key_errors_with_friendly_message() {
    let tmp = tempdir();
    let csv_path = tmp.join("ap.csv");
    let chart_path = tmp.join("accounts.txt");
    std::fs::write(
        &chart_path,
        "Expenses:通信費\nExpenses:消耗品費\nLiabilities:買掛金\n",
    )
    .unwrap();

    let jsonl = serde_json::json!({
        "source": "gmail",
        "external_id": "x",
        "attachment_path": "/p.pdf",
        "extracted": {
            "transaction_date": "2026-04-30",
            "total_amount_jpy": 1,
            "counterparty_name": "X",
            "confidence": 1.0
        },
        "status": "ok"
    })
    .to_string()
        + "\n";

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env_remove("ANTHROPIC_API_KEY")
        .args([
            "export",
            "gnucash",
            "--out",
            csv_path.to_str().unwrap(),
            "--accounts",
            chart_path.to_str().unwrap(),
            "--credit-account",
            "Liabilities:買掛金",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ANTHROPIC_API_KEY"), "got: {err}");
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-export-gnucash-doc-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
