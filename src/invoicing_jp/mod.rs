//! Japanese qualified-invoice (適格請求書) helpers: T number validation,
//! NTA registry API client, and (later) 電帳法 sidecar / CSV / 事務処理規程
//! generators.

pub mod tnumber;

/// Validate a T number against the National Tax Agency public registry.
///
/// Steps performed:
/// 1. Format check (`T` + 13 digits)
/// 2. Hit the NTA Web API to confirm the number is currently registered
/// 3. Print a one-line summary on stdout (or non-zero exit on failure)
pub async fn verify_tnumber(value: &str) -> anyhow::Result<()> {
    if !tnumber::is_valid_format(value) {
        anyhow::bail!("invalid T number format: must be `T` + 13 digits, got {value:?}");
    }
    anyhow::bail!("verify-tnumber: NTA API client not implemented yet (see issue #26)")
}
