//! End-to-end test for `fetchdoc fetch dir`.
//!
//! Drops a couple of fake PDFs in a tempdir, runs the binary, and checks the
//! emitted JSONL plus `--move-to` semantics (idempotent re-runs).

use assert_cmd::Command;

const FAKE_PDF: &[u8] = b"%PDF-1.4\n% fetchdoc fetch dir test\n%%EOF\n";

#[test]
fn ingests_pdfs_and_emits_jsonl() {
    let tmp = tempdir("ingests");
    let dir = tmp.join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("amazon-receipt.pdf"), FAKE_PDF).unwrap();
    std::fs::write(dir.join("yodobashi.PDF"), FAKE_PDF).unwrap();
    std::fs::write(dir.join("not-a-pdf.txt"), b"ignore me").unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args(["fetch", "dir", "--dir", dir.to_str().unwrap(), "--quiet"])
        .assert()
        .success();

    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout was: {stdout}");

    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["source"], "dir");
        let id = v["external_id"].as_str().unwrap();
        assert!(id.starts_with("sha256:"), "external_id: {id}");
        assert_eq!(id.len(), "sha256:".len() + 64);
        let path = v["attachment_path"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists());
        assert!(v["source_meta"]["file_size"].as_u64().unwrap() > 0);
        assert!(v["source_meta"]["original_path"].is_string());
    }

    // Both files share the same content → same sha256 → same external_id.
    let ids: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["external_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn move_to_makes_reruns_idempotent() {
    let tmp = tempdir("move-to");
    let inbox = tmp.join("inbox");
    let archive = tmp.join("archive");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("nuro-bill.pdf"), FAKE_PDF).unwrap();

    // First run — file is moved into archive.
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "dir",
            "--dir",
            inbox.to_str().unwrap(),
            "--move-to",
            archive.to_str().unwrap(),
            "--quiet",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(
        !inbox.join("nuro-bill.pdf").exists(),
        "source should be moved"
    );

    // Now drop the *same* content back into the inbox (simulating a
    // re-download). Second run should detect the duplicate and emit nothing.
    std::fs::write(inbox.join("nuro-bill.pdf"), FAKE_PDF).unwrap();
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "dir",
            "--dir",
            inbox.to_str().unwrap(),
            "--move-to",
            archive.to_str().unwrap(),
            "--quiet",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(
        stdout.lines().count(),
        0,
        "duplicate content should be skipped"
    );
    // The duplicate stays in the inbox so the user can investigate / delete.
    assert!(inbox.join("nuro-bill.pdf").exists());
}

#[test]
fn since_in_the_future_filters_everything() {
    // We don't shell out to `touch` to forge mtimes — instead we lean on the
    // fact that any file we *just* created has mtime < a far-future cutoff.
    let tmp = tempdir("since");
    let dir = tmp.join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("recent.pdf"), FAKE_PDF).unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "dir",
            "--dir",
            dir.to_str().unwrap(),
            "--since",
            "2099-01-01",
            "--quiet",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.lines().count(), 0);
}

#[test]
fn limit_caps_record_count() {
    let tmp = tempdir("limit");
    let dir = tmp.join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    // Three *distinct* contents so each gets its own external_id.
    std::fs::write(dir.join("a.pdf"), b"%PDF-1.4\nA\n%%EOF\n").unwrap();
    std::fs::write(dir.join("b.pdf"), b"%PDF-1.4\nB\n%%EOF\n").unwrap();
    std::fs::write(dir.join("c.pdf"), b"%PDF-1.4\nC\n%%EOF\n").unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "dir",
            "--dir",
            dir.to_str().unwrap(),
            "--limit",
            "2",
            "--quiet",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.lines().count(), 2);
}

#[test]
fn include_ext_overrides_default() {
    let tmp = tempdir("include-ext");
    let dir = tmp.join("inbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ignore.pdf"), FAKE_PDF).unwrap();
    std::fs::write(dir.join("invoice.png"), b"PNGFAKE").unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "fetch",
            "dir",
            "--dir",
            dir.to_str().unwrap(),
            "--include-ext",
            "png",
            "--quiet",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert!(
        v["attachment_path"]
            .as_str()
            .unwrap()
            .ends_with("invoice.png")
    );
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-fetch-dir-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
