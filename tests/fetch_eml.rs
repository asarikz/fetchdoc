//! End-to-end test for `fetchdoc fetch eml`.
//!
//! Generates a couple of small `.eml` fixtures (a normal one with a
//! Message-ID header and one without), runs the binary against them, and
//! checks both the JSONL output and the cached PDF files.

use assert_cmd::Command;
use base64::Engine;

const MIN_PDF: &[u8] = b"%PDF-1.4\n%%EOF\n";

fn write_eml(
    path: &std::path::Path,
    headers: &[(&str, &str)],
    pdf_filename: &str,
    pdf_bytes: &[u8],
) {
    let boundary = "BOUNDARY42";
    let mut s = String::new();
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("MIME-Version: 1.0\r\n");
    s.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
    ));
    s.push_str(&format!("--{boundary}\r\n"));
    s.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
    s.push_str("body text\r\n");
    s.push_str(&format!("--{boundary}\r\n"));
    s.push_str(&format!(
        "Content-Type: application/pdf; name=\"{pdf_filename}\"\r\n"
    ));
    s.push_str(&format!(
        "Content-Disposition: attachment; filename=\"{pdf_filename}\"\r\n"
    ));
    s.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    s.push_str(&base64::engine::general_purpose::STANDARD.encode(pdf_bytes));
    s.push_str("\r\n");
    s.push_str(&format!("--{boundary}--\r\n"));
    std::fs::write(path, s).unwrap();
}

#[test]
fn extracts_pdf_and_emits_jsonl() {
    let tmp = tempdir("extracts");
    let eml_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    write_eml(
        &eml_dir.join("a.eml"),
        &[
            ("From", "vendor@example.com"),
            ("To", "me@example.com"),
            ("Subject", "April invoice"),
            ("Date", "Wed, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<inv-001@example.com>"),
        ],
        "invoice.pdf",
        MIN_PDF,
    );

    // Nested subdirectory to prove recursion works.
    let sub = eml_dir.join("subdir");
    std::fs::create_dir_all(&sub).unwrap();
    write_eml(
        &sub.join("b.EML"), // upper-case extension to prove case-insensitive match
        &[
            ("From", "shop@example.com"),
            ("Subject", "May receipt"),
            ("Date", "Fri, 01 May 2026 12:34:56 +0900"),
            // No Message-ID — falls back to sha256 fingerprint.
        ],
        "receipt.pdf",
        b"%PDF-1.4\n%%no-msg-id\n%%EOF\n",
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "eml",
            "--dir",
            eml_dir.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run fetch eml");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 records, got: {stdout}");

    let mut by_subject: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for l in &lines {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        let subj = v["source_meta"]["subject"].as_str().unwrap().to_string();
        by_subject.insert(subj, v);
    }

    let april = &by_subject["April invoice"];
    assert_eq!(april["source"], "eml");
    assert_eq!(april["external_id"], "inv-001@example.com");
    assert_eq!(april["status"], "ok");
    assert_eq!(april["source_meta"]["from"], "vendor@example.com");
    assert_eq!(april["source_meta"]["attachment_filename"], "invoice.pdf");
    let april_path = april["attachment_path"].as_str().unwrap();
    assert!(april_path.ends_with(".pdf"));
    assert_eq!(std::fs::read(april_path).unwrap(), MIN_PDF);

    let may = &by_subject["May receipt"];
    let may_id = may["external_id"].as_str().unwrap();
    assert_eq!(may_id.len(), 16, "fallback id should be 16 hex chars");
    assert!(may_id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn since_filters_older_messages() {
    let tmp = tempdir("since");
    let eml_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    write_eml(
        &eml_dir.join("old.eml"),
        &[
            ("Subject", "old"),
            ("Date", "Mon, 01 Mar 2026 10:00:00 +0900"),
            ("Message-ID", "<old@example.com>"),
        ],
        "old.pdf",
        MIN_PDF,
    );
    write_eml(
        &eml_dir.join("new.eml"),
        &[
            ("Subject", "new"),
            ("Date", "Wed, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<new@example.com>"),
        ],
        "new.pdf",
        MIN_PDF,
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "eml",
            "--dir",
            eml_dir.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--since",
            "2026-04-01",
            "--quiet",
        ])
        .output()
        .expect("run fetch eml");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "only the April message should survive");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["source_meta"]["subject"], "new");
}

#[test]
fn limit_caps_emitted_records() {
    let tmp = tempdir("limit");
    let eml_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    for i in 0..5 {
        let mid = format!("<m{i:02}@example.com>");
        write_eml(
            &eml_dir.join(format!("m{i:02}.eml")),
            &[
                ("Subject", "x"),
                ("Date", "Wed, 30 Apr 2026 10:00:00 +0900"),
                ("Message-ID", &mid),
            ],
            "x.pdf",
            MIN_PDF,
        );
    }

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "eml",
            "--dir",
            eml_dir.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--limit",
            "2",
            "--quiet",
        ])
        .output()
        .expect("run fetch eml");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
}

#[test]
fn unreadable_eml_is_skipped_not_fatal() {
    let tmp = tempdir("unreadable");
    let eml_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    // A bogus "eml" file that mailparse cannot make sense of.
    std::fs::write(eml_dir.join("garbage.eml"), b"\x00\x01\x02 not a mail").unwrap();
    write_eml(
        &eml_dir.join("good.eml"),
        &[
            ("Subject", "ok"),
            ("Date", "Wed, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<g@example.com>"),
        ],
        "ok.pdf",
        MIN_PDF,
    );

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "eml",
            "--dir",
            eml_dir.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
        ])
        .output()
        .expect("run fetch eml");
    // The bad file is skipped with a stderr warning; the run still succeeds.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
}

/// Per-test unique tempdir. Includes the test name in the path so parallel
/// tests in this file (which share the nanosecond clock) cannot clobber each
/// other's `--dir` and `--cache-dir`.
fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-fetch-eml-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
