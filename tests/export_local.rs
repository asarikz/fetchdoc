//! End-to-end test for `fetchdoc export local`.
//!
//! Pipes a small Document JSONL stream into the binary, then verifies that
//! files were copied to the templated destinations and the JSONL passthrough
//! gained an `exported.local.path` pointer.

use assert_cmd::Command;

#[test]
fn copies_attachments_with_compliant_filename() {
    let tmp = tempdir();
    let src_a = tmp.join("a.pdf");
    let src_b = tmp.join("b.pdf");
    std::fs::write(&src_a, b"%PDF-A").unwrap();
    std::fs::write(&src_b, b"%PDF-B").unwrap();

    let root = tmp.join("out");

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::json!({
            "source": "gmail",
            "external_id": "msg-1",
            "attachment_path": src_a.to_str().unwrap(),
            "extracted": {
                "transaction_date": "2026-04-30",
                "total_amount_jpy": 12100,
                "counterparty_name": "アクメ商事",
                "confidence": 0.9
            },
            "status": "ok"
        }),
        serde_json::json!({
            "source": "gmail",
            "external_id": "msg-2",
            "attachment_path": src_b.to_str().unwrap(),
            "extracted": {
                "transaction_date": "2026-05-01",
                "total_amount_jpy": 500,
                "counterparty_name": "B社",
                "confidence": 0.8
            },
            "status": "ok"
        })
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "export",
            "local",
            "--root",
            root.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export local");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dest_a = root.join("2026-04-30_アクメ商事_12100円.pdf");
    let dest_b = root.join("2026-05-01_B社_500円.pdf");
    assert_eq!(std::fs::read(&dest_a).unwrap(), b"%PDF-A");
    assert_eq!(std::fs::read(&dest_b).unwrap(), b"%PDF-B");

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["exported"]["local"]["path"], dest_a.to_str().unwrap());
    assert_eq!(first["status"], "ok");
}

#[test]
fn missing_extracted_marks_needs_review_and_continues() {
    let tmp = tempdir();
    let src = tmp.join("c.pdf");
    std::fs::write(&src, b"%PDF-C").unwrap();
    let root = tmp.join("out");

    let jsonl = format!(
        "{}\n{}\n",
        // No `extracted` — should be skipped (no copy) and re-emitted with
        // status=needs_review.
        serde_json::json!({
            "source": "gmail",
            "external_id": "no-classify",
            "attachment_path": src.to_str().unwrap(),
            "status": "ok"
        }),
        // Fully populated — should still be copied.
        serde_json::json!({
            "source": "gmail",
            "external_id": "ok",
            "attachment_path": src.to_str().unwrap(),
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
            "local",
            "--root",
            root.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export local");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(first["status"], "needs_review");
    assert!(first.get("exported").is_none());

    let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(second["status"], "ok");
    assert!(second["exported"]["local"]["path"].is_string());
}

#[test]
fn template_can_fan_into_subdirectories() {
    let tmp = tempdir();
    let src = tmp.join("d.pdf");
    std::fs::write(&src, b"%PDF-D").unwrap();
    let root = tmp.join("out");

    let jsonl = serde_json::json!({
        "source": "gmail",
        "external_id": "fan",
        "attachment_path": src.to_str().unwrap(),
        "extracted": {
            "transaction_date": "2026-04-30",
            "total_amount_jpy": 1,
            "counterparty_name": "Y",
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
            "local",
            "--root",
            root.to_str().unwrap(),
            "--name-template",
            "{yyyy}/{mm}/{external_id}.pdf",
            "--quiet",
        ])
        .write_stdin(jsonl)
        .output()
        .expect("run export local");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dest = root.join("2026").join("04").join("fan.pdf");
    assert_eq!(std::fs::read(&dest).unwrap(), b"%PDF-D");
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-export-local-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
