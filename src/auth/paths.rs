//! Configuration paths.
//!
//! `~/.config/fetchdoc/` on Unix-likes (or `$XDG_CONFIG_HOME/fetchdoc/` if
//! set). Windows is best-effort: `%APPDATA%\fetchdoc\` if `APPDATA` is set,
//! otherwise we fall back to `~/.config/fetchdoc/`. We deliberately avoid
//! the `directories` crate to keep the dependency footprint small — the
//! only thing fetchdoc stores on disk is `client_secret.json`.

use anyhow::Context;
use std::path::PathBuf;

/// Directory holding fetchdoc's per-user config.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("fetchdoc"));
        }
    }
    if cfg!(windows) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return Ok(PathBuf::from(appdata).join("fetchdoc"));
            }
        }
    }
    let home = std::env::var("HOME")
        .context("neither XDG_CONFIG_HOME nor HOME is set; cannot locate config dir")?;
    Ok(PathBuf::from(home).join(".config").join("fetchdoc"))
}

/// Where `client_secret.json` lives once the user has run `auth init`.
pub fn client_secret_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("client_secret.json"))
}
