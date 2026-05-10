//! End-to-end test for `fetchdoc fetch mbox`.
//!
//! Builds a small mbox fixture in a tempdir (two messages, both with PDF
//! attachments; one missing Message-ID), runs the binary, and verifies the
//! JSONL records and on-disk PDFs.

use assert_cmd::Command;
use base64::Engine;

const MIN_PDF: &[u8] = b"%PDF-1.4\n%%EOF\n";

fn message(headers: &[(&str, &str)], pdf_filename: &str, pdf_bytes: &[u8]) -> String {
    let boundary = "BDY";
    let mut s = String::new();
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s.push_str("MIME-Version: 1.0\n");
    s.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\n\n"
    ));
    s.push_str(&format!("--{boundary}\n"));
    s.push_str("Content-Type: text/plain; charset=utf-8\n\n");
    s.push_str("body text\n");
    s.push_str(&format!("--{boundary}\n"));
    s.push_str(&format!(
        "Content-Type: application/pdf; name=\"{pdf_filename}\"\n"
    ));
    s.push_str(&format!(
        "Content-Disposition: attachment; filename=\"{pdf_filename}\"\n"
    ));
    s.push_str("Content-Transfer-Encoding: base64\n\n");
    s.push_str(&base64::engine::general_purpose::STANDARD.encode(pdf_bytes));
    s.push('\n');
    s.push_str(&format!("--{boundary}--\n"));
    s
}

fn write_mbox(path: &std::path::Path, messages: &[(String, String)]) {
    // messages is a list of (From-line-tail, message-body)
    let mut out = String::new();
    for (from_line, body) in messages {
        out.push_str(&format!("From {from_line}\n"));
        out.push_str(body);
        // Blank line between messages — what a real mboxrd writer emits.
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    std::fs::write(path, out).unwrap();
}

#[test]
fn extracts_pdfs_from_two_message_mbox() {
    let tmp = tempdir("two-msg");
    let mbox_path = tmp.join("inbox.mbox");
    let cache_dir = tmp.join("cache");

    let m1 = message(
        &[
            ("From", "vendor@example.com"),
            ("To", "me@example.com"),
            ("Subject", "April invoice"),
            ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<inv-001@example.com>"),
        ],
        "invoice.pdf",
        MIN_PDF,
    );
    let m2 = message(
        &[
            // No Message-ID → fallback id (16 hex chars).
            ("From", "shop@example.com"),
            ("Subject", "May receipt"),
            ("Date", "Fri, 01 May 2026 12:34:56 +0900"),
        ],
        "receipt.pdf",
        b"%PDF-1.4\n%%no-msg-id\n%%EOF\n",
    );

    write_mbox(
        &mbox_path,
        &[
            (
                "vendor@example.com Thu Apr 30 10:00:00 2026".to_string(),
                m1,
            ),
            ("shop@example.com Fri May 01 12:34:56 2026".to_string(), m2),
        ],
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "mbox",
            "--file",
            mbox_path.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run fetch mbox");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 records, got: {stdout}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["source"], "mbox");
    assert_eq!(first["external_id"], "inv-001@example.com");
    assert_eq!(first["status"], "ok");
    assert_eq!(first["source_meta"]["subject"], "April invoice");
    assert_eq!(first["source_meta"]["mbox_index"], 0);
    assert_eq!(
        first["source_meta"]["mbox_path"],
        mbox_path.to_string_lossy().as_ref()
    );
    let p = first["attachment_path"].as_str().unwrap();
    assert_eq!(std::fs::read(p).unwrap(), MIN_PDF);

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["source_meta"]["mbox_index"], 1);
    let id = second["external_id"].as_str().unwrap();
    assert_eq!(id.len(), 16);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn dir_picks_up_apple_mail_bundle_layout() {
    let tmp = tempdir("dir");
    let cache_dir = tmp.join("cache");

    // Apple Mail layout: `Inbox.mbox/mbox` (bare "mbox" inside `*.mbox/`).
    let bundle = tmp.join("Inbox.mbox");
    std::fs::create_dir_all(&bundle).unwrap();
    let inner = bundle.join("mbox");

    let m = message(
        &[
            ("From", "v@example.com"),
            ("Subject", "S"),
            ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<apple@example.com>"),
        ],
        "x.pdf",
        MIN_PDF,
    );
    write_mbox(
        &inner,
        &[("v@example.com Thu Apr 30 10:00:00 2026".to_string(), m)],
    );

    // Plus a sibling `*.mbox` file in the same root to prove dir-walking.
    let thunder = tmp.join("Sent.mbox");
    let m2 = message(
        &[
            ("From", "v2@example.com"),
            ("Subject", "S2"),
            ("Date", "Fri, 01 May 2026 10:00:00 +0900"),
            ("Message-ID", "<thunder@example.com>"),
        ],
        "y.pdf",
        MIN_PDF,
    );
    write_mbox(
        &thunder,
        &[("v2@example.com Fri May 01 10:00:00 2026".to_string(), m2)],
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "mbox",
            "--dir",
            tmp.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run fetch mbox");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let ids: std::collections::HashSet<String> = stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["external_id"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(
        ids.contains("apple@example.com"),
        "missing apple id: {ids:?}"
    );
    assert!(
        ids.contains("thunder@example.com"),
        "missing thunder id: {ids:?}"
    );
}

#[test]
fn since_filters_older_messages() {
    let tmp = tempdir("since");
    let mbox_path = tmp.join("inbox.mbox");
    let cache_dir = tmp.join("cache");

    let old = message(
        &[
            ("From", "a@example.com"),
            ("Subject", "old"),
            ("Date", "Sun, 01 Mar 2026 10:00:00 +0900"),
            ("Message-ID", "<old@example.com>"),
        ],
        "old.pdf",
        MIN_PDF,
    );
    let new = message(
        &[
            ("From", "a@example.com"),
            ("Subject", "new"),
            ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<new@example.com>"),
        ],
        "new.pdf",
        MIN_PDF,
    );

    write_mbox(
        &mbox_path,
        &[
            ("a@example.com Sun Mar 01 10:00:00 2026".to_string(), old),
            ("a@example.com Thu Apr 30 10:00:00 2026".to_string(), new),
        ],
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "mbox",
            "--file",
            mbox_path.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--since",
            "2026-04-01",
            "--quiet",
        ])
        .output()
        .expect("run fetch mbox");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["source_meta"]["subject"], "new");
}

#[test]
fn limit_caps_emitted_records() {
    let tmp = tempdir("limit");
    let mbox_path = tmp.join("inbox.mbox");
    let cache_dir = tmp.join("cache");

    let mut msgs = Vec::new();
    for i in 0..5 {
        let mid = format!("<m{i:02}@example.com>");
        let m = message(
            &[
                ("From", "a@example.com"),
                ("Subject", "x"),
                ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
                ("Message-ID", &mid),
            ],
            "x.pdf",
            MIN_PDF,
        );
        msgs.push(("a@example.com Thu Apr 30 10:00:00 2026".to_string(), m));
    }
    write_mbox(&mbox_path, &msgs);

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "mbox",
            "--file",
            mbox_path.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--limit",
            "2",
            "--quiet",
        ])
        .output()
        .expect("run fetch mbox");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
}

#[test]
fn requires_file_or_dir() {
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args(["fetch", "mbox"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--file") || err.contains("--dir"),
        "got: {err}"
    );
}

/// Per-test unique tempdir; the per-test `tag` defends against nanosecond
/// collisions when this file's tests run in parallel.
fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-fetch-mbox-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
