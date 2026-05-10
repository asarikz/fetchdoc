//! Token storage in the OS keychain via the `keyring` crate.
//!
//! - macOS: Keychain
//! - Windows: Credential Manager
//! - Linux: Secret Service (gnome-keyring / kwallet)
//!
//! Account names are namespaced as `<source>:<identity>` (e.g.
//! `gmail:user@example.com`). For v0.1 we use the literal `default` identity
//! since fetchdoc does not yet support multiple accounts per source.

use anyhow::Context;
use keyring::Entry;

const SERVICE: &str = "fetchdoc";

/// Store a refresh token under `account` (e.g. `"gmail:default"`).
pub fn store(account: &str, token: &str) -> anyhow::Result<()> {
    Entry::new(SERVICE, account)
        .with_context(|| format!("opening keyring entry for {account}"))?
        .set_password(token)
        .with_context(|| format!("writing token for {account}"))?;
    Ok(())
}

/// Load a previously stored refresh token, or `None` if absent.
pub fn load(account: &str) -> anyhow::Result<Option<String>> {
    let entry = Entry::new(SERVICE, account)
        .with_context(|| format!("opening keyring entry for {account}"))?;
    match entry.get_password() {
        Ok(t) => Ok(Some(t)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading token for {account}")),
    }
}

/// Remove the stored credential. Idempotent: returns `Ok` whether or not an
/// entry existed.
pub fn delete(account: &str) -> anyhow::Result<()> {
    let entry = Entry::new(SERVICE, account)
        .with_context(|| format!("opening keyring entry for {account}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting token for {account}")),
    }
}
