//! Token storage in the OS keychain via the `keyring` crate.
//!
//! - macOS: Keychain
//! - Windows: Credential Manager
//! - Linux: Secret Service (gnome-keyring / kwallet)

#![allow(dead_code)] // Stub — wired up in the auth implementation issue.

const SERVICE: &str = "fetchdoc";

/// Store a refresh token under `account` (e.g. `"gmail:user@example.com"`).
pub fn store(_account: &str, _token: &str) -> anyhow::Result<()> {
    unimplemented!("storage::store")
}

/// Load a previously stored refresh token, or `None` if absent.
pub fn load(_account: &str) -> anyhow::Result<Option<String>> {
    unimplemented!("storage::load")
}

/// Remove the stored credential (idempotent).
pub fn delete(_account: &str) -> anyhow::Result<()> {
    unimplemented!("storage::delete")
}
