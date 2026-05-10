//! Integration tests for `fetchdoc auth init`.
//!
//! We point `XDG_CONFIG_HOME` at a tmp directory so the test never touches
//! the user's real `~/.config/fetchdoc/`. The keychain-bound subcommands
//! (`login`, `status`, `logout`) are not covered here because they would
//! require a working Secret Service / Keychain on every CI runner.

use assert_cmd::Command;

#[test]
fn init_with_from_copies_desktop_client_secret() {
    let tmp = tempdir("desktop");
    let src = tmp.join("downloaded.json");
    std::fs::write(
        &src,
        r#"{"installed":{"client_id":"abc","client_secret":"shh"}}"#,
    )
    .unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("XDG_CONFIG_HOME", &tmp)
        .args(["auth", "init", "--from", src.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dest = tmp.join("fetchdoc").join("client_secret.json");
    assert!(dest.exists(), "client_secret.json was not copied");
    assert!(
        std::fs::read_to_string(&dest)
            .unwrap()
            .contains("client_id")
    );
}

#[test]
fn init_rejects_web_client_secret_without_writing() {
    let tmp = tempdir("web");
    let src = tmp.join("web.json");
    std::fs::write(&src, r#"{"web":{"client_id":"abc","client_secret":"shh"}}"#).unwrap();

    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("XDG_CONFIG_HOME", &tmp)
        .args(["auth", "init", "--from", src.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Desktop"), "got: {err}");
    let dest = tmp.join("fetchdoc").join("client_secret.json");
    assert!(!dest.exists(), "should not have copied a Web client_secret");
}

#[test]
fn init_without_from_prints_setup_instructions() {
    let tmp = tempdir("setup");
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("XDG_CONFIG_HOME", &tmp)
        .args(["auth", "init"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("console.cloud.google.com"), "got: {err}");
    assert!(err.contains("Desktop app"), "got: {err}");
    assert!(err.contains("--from"), "got: {err}");
}

#[test]
fn login_without_client_secret_errors_clearly() {
    let tmp = tempdir("login");
    let out = Command::cargo_bin("fetchdoc")
        .unwrap()
        .env("XDG_CONFIG_HOME", &tmp)
        .args(["auth", "login", "--source", "gmail"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("auth init"), "got: {err}");
}

/// Per-test unique tempdir. The `tag` prefix defends against nanosecond
/// clock collisions when these tests run in parallel — without it, two
/// tests can land in the same path and clobber each other's
/// `XDG_CONFIG_HOME`, which then makes one fail intermittently in CI.
fn tempdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("fetchdoc-auth-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}
