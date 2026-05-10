//! End-to-end coverage of the body-primary path:
//!   1. `fetch eml` emits a body-primary record when an `.eml` has no PDF
//!      attachment but a usable text body — `attachment_path` is None and
//!      `source_meta` carries the body-primary marker fields.
//!   2. `fetch mbox` materialises an `.eml` in the cache for body-primary
//!      messages (mbox can't reuse on-disk files like `fetch eml` can).
//!   3. `render-body` (with the test-only `FETCHDOC_RENDER_BODY_FAKE` env
//!      var so CI doesn't need chromium) consumes those records, writes
//!      a stub PDF, and emits Documents with `attachment_path` filled.
//!   4. Records that already have an `attachment_path` (the normal PDF
//!      case) are passed through `render-body` untouched.

use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;

fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-render-body-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_html_only_eml(path: &Path) {
    let s = "From: billing@example.com\r\n\
             To: me@example.com\r\n\
             Subject: April subscription receipt\r\n\
             Date: Wed, 30 Apr 2026 10:00:00 +0900\r\n\
             Message-ID: <html-only-001@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Thanks! Total: <b>¥12,100</b></p>\r\n";
    std::fs::write(path, s).unwrap();
}

fn write_plain_only_eml(path: &Path) {
    let s = "From: billing@example.com\r\n\
             Subject: May invoice (text)\r\n\
             Date: Fri, 01 May 2026 09:00:00 +0900\r\n\
             Message-ID: <plain-only-001@example.com>\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Total: 5,500 JPY\r\n";
    std::fs::write(path, s).unwrap();
}

#[test]
fn fetch_eml_emits_body_primary_record_when_no_pdf() {
    let tmp = tempdir("fetch-eml-body");
    let eml_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    let html_eml = eml_dir.join("html.eml");
    write_html_only_eml(&html_eml);

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

    let lines: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let v: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["source"], "eml");
    assert_eq!(v["external_id"], "html-only-001@example.com");
    assert!(
        v.get("attachment_path").is_none(),
        "body-primary records have no attachment_path yet: {v}"
    );
    let meta = &v["source_meta"];
    assert_eq!(meta["body_is_primary"], true);
    assert_eq!(meta["body_mime_type"], "text/html");
    // For `fetch eml` the eml_path points at the user-provided file (no copy
    // into the cache) — that's the integration contract.
    assert_eq!(
        meta["eml_path"].as_str().unwrap(),
        html_eml.to_str().unwrap()
    );
    assert!(meta["body_part_index"].as_u64().is_some());
}

#[test]
fn fetch_mbox_body_primary_materialises_eml_in_cache() {
    let tmp = tempdir("fetch-mbox-body");
    let mbox_dir = tmp.join("mail");
    let cache_dir = tmp.join("cache");
    std::fs::create_dir_all(&mbox_dir).unwrap();

    // A one-message mbox archive with a plain-text body and no PDF.
    let mbox = mbox_dir.join("inbox.mbox");
    let s = "From sender@example.com Wed Apr 30 10:00:00 2026\n\
             From: shop@example.com\n\
             Subject: Receipt body only\n\
             Date: Wed, 30 Apr 2026 10:00:00 +0900\n\
             Message-ID: <mbox-body-001@example.com>\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             Total: 1,234 JPY\n";
    std::fs::write(&mbox, s).unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "mbox",
            "--file",
            mbox.to_str().unwrap(),
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

    let lines: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1);
    let v: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["source_meta"]["body_is_primary"], true);

    // mbox can't point at an on-disk per-message .eml — body_primary_record
    // must have written one into the cache dir.
    let eml_path = v["source_meta"]["eml_path"].as_str().unwrap();
    assert!(
        Path::new(eml_path).starts_with(&cache_dir),
        "expected cached eml under {} but got {eml_path}",
        cache_dir.display()
    );
    assert!(Path::new(eml_path).exists());
}

#[test]
fn render_body_pipes_body_primary_record_into_pdf_with_fake_renderer() {
    let tmp = tempdir("render-pipeline");
    let eml_dir = tmp.join("mail");
    let fetch_cache = tmp.join("fetch-cache");
    let render_cache = tmp.join("render-cache");
    std::fs::create_dir_all(&eml_dir).unwrap();

    write_plain_only_eml(&eml_dir.join("a.eml"));

    let fetch_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "eml",
            "--dir",
            eml_dir.to_str().unwrap(),
            "--cache-dir",
            fetch_cache.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .expect("run fetch eml");
    assert!(fetch_out.status.success());

    let render_out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("FETCHDOC_RENDER_BODY_FAKE", "1")
        .args([
            "render-body",
            "--cache-dir",
            render_cache.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(fetch_out.stdout)
        .output()
        .expect("run render-body");
    assert!(
        render_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&render_out.stderr)
    );

    let lines: Vec<&str> = std::str::from_utf8(&render_out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1);
    let v: Value = serde_json::from_str(lines[0]).unwrap();

    let pdf = v["attachment_path"]
        .as_str()
        .expect("attachment_path filled");
    assert!(
        Path::new(pdf).starts_with(&render_cache),
        "pdf should live under the render cache dir, got {pdf}"
    );
    let bytes = std::fs::read(pdf).unwrap();
    assert!(bytes.starts_with(b"%PDF"), "stub PDF was written");

    // The HTML envelope produced for the renderer is also kept around
    // (useful for debugging / re-rendering with a different backend).
    let html = v["source_meta"]["body_html_path"]
        .as_str()
        .expect("body_html_path recorded");
    assert!(Path::new(html).exists());
    let html_text = std::fs::read_to_string(html).unwrap();
    assert!(
        html_text.contains("Total: 5,500 JPY"),
        "body content rendered into HTML"
    );
    assert!(
        html_text.contains("billing@example.com"),
        "header includes From"
    );
}

#[test]
fn render_body_passes_through_records_that_already_have_pdfs() {
    let tmp = tempdir("render-passthrough");
    let render_cache = tmp.join("render-cache");

    // A Document JSONL line that already has an attachment_path — render-body
    // must emit it unchanged. (We don't even need a real PDF on disk; the
    // command shouldn't touch it.)
    let input = r#"{"source":"eml","external_id":"already@x","attachment_path":"/tmp/already.pdf","source_meta":{"subject":"x"},"status":"ok"}
"#;

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("FETCHDOC_RENDER_BODY_FAKE", "1")
        .args([
            "render-body",
            "--cache-dir",
            render_cache.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(input)
        .output()
        .expect("run render-body");
    assert!(out.status.success());

    let line = std::str::from_utf8(&out.stdout).unwrap().trim();
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["attachment_path"], "/tmp/already.pdf");
    assert!(v["source_meta"].get("body_html_path").is_none());
}
