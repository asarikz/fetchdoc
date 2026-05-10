//! End-to-end test for `fetchdoc import dedup`.

use assert_cmd::Command;
use std::io::Write;

#[test]
fn drops_seen_external_ids_keeps_new_ones() {
    let tmp = tempdir();
    let history = tmp.join("history.jsonl");
    {
        let mut f = std::fs::File::create(&history).unwrap();
        // One previously-imported row.
        writeln!(
            f,
            r#"{{"source":"csv","external_id":"csv:smbc:2026-04-30:abc","posted_date":"2026-04-30","amount_jpy":-12100,"description_raw":"Acme","status":"ok"}}"#
        )
        .unwrap();
    }

    // Stream: the same row + a new one. Expect only the new one to come out.
    let stdin = "\
{\"source\":\"csv\",\"external_id\":\"csv:smbc:2026-04-30:abc\",\"posted_date\":\"2026-04-30\",\"amount_jpy\":-12100,\"description_raw\":\"Acme\",\"status\":\"ok\"}
{\"source\":\"csv\",\"external_id\":\"csv:smbc:2026-05-01:def\",\"posted_date\":\"2026-05-01\",\"amount_jpy\":500000,\"description_raw\":\"Salary\",\"status\":\"ok\"}
";

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "dedup",
            "--against",
            history.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 row to survive: {stdout}");
    let kept: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(kept["external_id"], "csv:smbc:2026-05-01:def");
}

#[test]
fn dedups_within_input_stream_too() {
    let tmp = tempdir();
    let empty_history = tmp.join("empty.jsonl");
    std::fs::write(&empty_history, "").unwrap();

    // Same external_id appears twice in the input.
    let stdin = "\
{\"source\":\"csv\",\"external_id\":\"x\",\"posted_date\":\"2026-04-30\",\"amount_jpy\":-100,\"description_raw\":\"a\",\"status\":\"ok\"}
{\"source\":\"csv\",\"external_id\":\"x\",\"posted_date\":\"2026-04-30\",\"amount_jpy\":-100,\"description_raw\":\"a\",\"status\":\"ok\"}
";

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .args([
            "import",
            "dedup",
            "--against",
            empty_history.to_str().unwrap(),
            "--quiet",
        ])
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert!(out.status.success());
    let n = String::from_utf8(out.stdout).unwrap().lines().count();
    assert_eq!(n, 1, "duplicate within stdin should also be dropped");
}

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
