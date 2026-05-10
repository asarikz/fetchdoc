//! End-to-end test for `fetchdoc fetch maildir`.
//!
//! Builds a Maildir++ tree on disk (INBOX + a `.Sent` subfolder, each with
//! `cur/`, `new/`, `tmp/`), drops a few raw RFC 822 messages with PDF
//! attachments inside, and verifies the JSONL records and on-disk PDFs.

use assert_cmd::Command;
use base64::Engine;

const MIN_PDF: &[u8] = b"%PDF-1.4\n%%EOF\n";

fn message(headers: &[(&str, &str)], pdf_filename: &str, pdf_bytes: &[u8]) -> String {
    let boundary = "BDY";
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
    s
}

fn make_maildir(root: &std::path::Path) {
    for sub in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
}

#[test]
fn extracts_pdfs_from_inbox_and_sent_folder() {
    let tmp = tempdir("md");
    let cache_dir = tmp.join("cache");
    let mail = tmp.join("Mail");
    make_maildir(&mail);
    make_maildir(&mail.join(".Sent"));

    // INBOX/cur — typical maildir filename with the `:2,S` (Seen) suffix.
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
    std::fs::write(mail.join("cur").join("1714435200.M1.host:2,S"), m1).unwrap();

    // INBOX/new — no Message-ID, so fallback id kicks in.
    let m2 = message(
        &[
            ("From", "shop@example.com"),
            ("Subject", "May receipt"),
            ("Date", "Fri, 01 May 2026 12:34:56 +0900"),
        ],
        "receipt.pdf",
        b"%PDF-1.4\n%%no-msg-id\n%%EOF\n",
    );
    std::fs::write(mail.join("new").join("1714521600.M2.host"), m2).unwrap();

    // .Sent/cur — picked up by Maildir++ recursion.
    let m3 = message(
        &[
            ("From", "me@example.com"),
            ("Subject", "sent reply"),
            ("Date", "Sat, 02 May 2026 09:00:00 +0900"),
            ("Message-ID", "<sent-001@example.com>"),
        ],
        "spec.pdf",
        MIN_PDF,
    );
    std::fs::write(
        mail.join(".Sent")
            .join("cur")
            .join("1714608000.M3.host:2,S"),
        m3,
    )
    .unwrap();

    // tmp/ should be ignored entirely.
    let trash = message(
        &[
            ("Subject", "half written"),
            ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<tmp@example.com>"),
        ],
        "tmp.pdf",
        MIN_PDF,
    );
    std::fs::write(mail.join("tmp").join("1714435200.M99.host"), trash).unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "maildir",
            "--dir",
            mail.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run fetch maildir");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 records, got: {stdout}");

    // Build a {subject -> record} map so the assertions don't depend on
    // walk order.
    let by_subject: std::collections::HashMap<String, serde_json::Value> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            (v["source_meta"]["subject"].as_str().unwrap().to_string(), v)
        })
        .collect();

    // tmp/ message must have been ignored.
    assert!(!by_subject.contains_key("half written"));

    let april = &by_subject["April invoice"];
    assert_eq!(april["source"], "maildir");
    assert_eq!(april["external_id"], "inv-001@example.com");
    assert_eq!(april["status"], "ok");
    assert_eq!(
        april["source_meta"]["maildir_path"],
        mail.to_string_lossy().as_ref()
    );
    let p = april["attachment_path"].as_str().unwrap();
    assert_eq!(std::fs::read(p).unwrap(), MIN_PDF);

    let may = &by_subject["May receipt"];
    let id = may["external_id"].as_str().unwrap();
    assert_eq!(id.len(), 16);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

    let sent = &by_subject["sent reply"];
    assert_eq!(
        sent["source_meta"]["maildir_path"],
        mail.join(".Sent").to_string_lossy().as_ref()
    );
}

#[test]
fn since_filters_older_messages() {
    let tmp = tempdir("since");
    let cache_dir = tmp.join("cache");
    let mail = tmp.join("Mail");
    make_maildir(&mail);

    let old = message(
        &[
            ("Subject", "old"),
            ("Date", "Sun, 01 Mar 2026 10:00:00 +0900"),
            ("Message-ID", "<old@example.com>"),
        ],
        "o.pdf",
        MIN_PDF,
    );
    std::fs::write(mail.join("cur").join("1.M.h:2,"), old).unwrap();
    let new = message(
        &[
            ("Subject", "new"),
            ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
            ("Message-ID", "<new@example.com>"),
        ],
        "n.pdf",
        MIN_PDF,
    );
    std::fs::write(mail.join("cur").join("2.M.h:2,"), new).unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "maildir",
            "--dir",
            mail.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--since",
            "2026-04-01",
            "--quiet",
        ])
        .output()
        .expect("run fetch maildir");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["source_meta"]["subject"], "new");
}

#[test]
fn errors_when_dir_is_not_a_maildir() {
    let tmp = tempdir("not-md");
    let mail = tmp.join("not-mail");
    std::fs::create_dir_all(&mail).unwrap();
    // Just an arbitrary directory with no cur/new/tmp anywhere.
    std::fs::write(mail.join("a.txt"), b"hi").unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args(["fetch", "maildir", "--dir", mail.to_str().unwrap()])
        .output()
        .expect("run fetch maildir");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no Maildir found"), "got: {err}");
}

#[test]
fn limit_caps_emitted_records() {
    let tmp = tempdir("limit");
    let cache_dir = tmp.join("cache");
    let mail = tmp.join("Mail");
    make_maildir(&mail);

    for i in 0..5 {
        let mid = format!("<m{i:02}@example.com>");
        let m = message(
            &[
                ("Subject", "x"),
                ("Date", "Thu, 30 Apr 2026 10:00:00 +0900"),
                ("Message-ID", &mid),
            ],
            "x.pdf",
            MIN_PDF,
        );
        std::fs::write(mail.join("cur").join(format!("{i:02}.M.h:2,")), m).unwrap();
    }

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "maildir",
            "--dir",
            mail.to_str().unwrap(),
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--limit",
            "2",
            "--quiet",
        ])
        .output()
        .expect("run fetch maildir");
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-fetch-maildir-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
