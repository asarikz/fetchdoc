//! Profile = per-source TOML mapping that tells the CSV parser:
//! - what encoding the file is in (Japanese banks still ship Shift_JIS)
//! - which delimiter / header row to use
//! - how each column maps to the [`Transaction`](crate::io::Transaction) schema
//! - whether amounts are signed (one column) or split across withdrawal/deposit
//!
//! Profiles are looked up by name in `~/.config/fetchdoc/profiles/<name>.toml`
//! or passed as a path. They are intentionally human-readable so users can
//! tweak them after `import csv --infer` generates one.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One profile, deserialised straight from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    /// Stable name used to identify this profile in records and on the CLI.
    pub name: String,

    /// File encoding label (`utf-8`, `shift_jis`, `cp932`, etc.). Defaults to UTF-8.
    #[serde(default = "default_encoding")]
    pub encoding: String,

    /// Field delimiter. Defaults to `,`. Use `"\t"` for TSV.
    #[serde(default = "default_delimiter")]
    pub delimiter: String,

    /// 1-indexed row number that contains the header. Defaults to `1`.
    /// Rows before this are silently skipped (some banks ship a few comment
    /// lines before the real header).
    #[serde(default = "default_header_row")]
    pub header_row: usize,

    /// Number of data rows to skip immediately after the header. Defaults to 0.
    #[serde(default)]
    pub skip_rows: usize,

    /// `chrono::format::strftime` pattern. Defaults to `%Y-%m-%d`.
    #[serde(default = "default_date_format")]
    pub date_format: String,

    /// Column-name mapping.
    pub columns: Columns,

    /// Optional secondary-source join config. When set, the importer expects
    /// `--dir <path>` instead of a single file: it reads files matching
    /// `multi.primary_glob` (using the top-level columns/encoding) and
    /// `multi.debit.glob`, joins them by (date ± window, total amount), and
    /// emits one [`Transaction`](crate::io::Transaction) per primary row with
    /// `splits` filled in for matched debit rows.
    #[serde(default)]
    pub multi: Option<MultiConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Columns {
    /// Column header for the posted/booking date. Required.
    pub posted_date: String,

    /// Column header for the description / memo line. Required.
    pub description: String,

    /// Single signed amount column. Set this **or** the
    /// `withdrawal`+`deposit` pair, not both.
    #[serde(default)]
    pub amount: Option<String>,

    /// Outflow column (positive number = money leaving the account).
    #[serde(default)]
    pub withdrawal: Option<String>,

    /// Inflow column (positive number = money entering the account).
    #[serde(default)]
    pub deposit: Option<String>,

    /// Running balance after the row. Optional.
    #[serde(default)]
    pub balance: Option<String>,

    /// Free-form memo column. Optional — distinct from `description` if the
    /// source provides both.
    #[serde(default)]
    pub memo: Option<String>,

    /// Value/effective date column. Optional.
    #[serde(default)]
    pub value_date: Option<String>,
}

/// Two-file join config (e.g. SBI Sumishin: bank statement + debit-card detail).
/// The top-level [`Profile`] describes the primary file; this struct adds the
/// secondary "detail" file and how to join them.
#[derive(Debug, Clone, Deserialize)]
pub struct MultiConfig {
    /// Glob (relative to `--dir`) matching the primary statement files.
    /// Example: `"nyushukinmeisai_*.csv"`.
    pub primary_glob: String,
    /// Regex applied to the primary `description` column. Only rows that match
    /// are joined against the debit file; non-matching rows pass through as
    /// plain (single-split) transactions. Capture groups are saved into
    /// `source_meta.debit_ref` (only the first named group, if any).
    #[serde(default)]
    pub primary_match_regex: Option<String>,
    /// Secondary "detail" file config.
    pub debit: DebitFile,
    /// Join parameters.
    #[serde(default)]
    pub join: JoinConfig,
}

/// Debit-card / detail-file profile. Independent encoding, headers, columns —
/// SBI Sumishin's two CSVs share Shift_JIS but other banks may differ.
#[derive(Debug, Clone, Deserialize)]
pub struct DebitFile {
    /// Glob (relative to `--dir`) matching the debit detail files.
    pub glob: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
    #[serde(default = "default_header_row")]
    pub header_row: usize,
    #[serde(default)]
    pub skip_rows: usize,
    #[serde(default = "default_date_format")]
    pub date_format: String,
    /// Optional row filter: keep only rows whose `column` cell equals `value`.
    /// SBI Sumishin's debit CSV has a leading marker column (header text `"1"`,
    /// data-row value `"2"`); set this to skip any non-`"2"` rows.
    #[serde(default)]
    pub row_filter: Option<RowFilter>,
    /// Column-name mapping for the debit file.
    pub columns: DebitColumns,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RowFilter {
    /// Column header text in the debit file.
    pub column: String,
    /// Required cell value; rows whose cell equals this are kept.
    pub value: String,
}

/// Column mapping for the debit detail file. All columns optional except
/// `date`, `merchant`, and `tx_amount` — those three are needed to join and
/// produce a useful split.
#[derive(Debug, Clone, Deserialize)]
pub struct DebitColumns {
    /// Transaction date on the debit slip (`お取引日`).
    pub date: String,
    /// Merchant / counterparty name (`お取引内容`).
    pub merchant: String,
    /// Settled JPY amount (`お取引金額`) — the principal in the bank's reporting currency.
    pub tx_amount: String,
    /// Per-transaction fee (`お取引手数料`). Optional; treated as 0 if missing or blank.
    #[serde(default)]
    pub tx_fee: Option<String>,
    /// ATM fee (`ATM手数料`). Optional.
    #[serde(default)]
    pub atm_fee: Option<String>,
    /// FX handling fee (`海外事務手数料`). Optional; if set and non-zero, gets its own split.
    #[serde(default)]
    pub fx_fee: Option<String>,
    /// Foreign currency code (`ご利用通貨`). Optional, recorded in `source_meta`.
    #[serde(default)]
    pub use_currency: Option<String>,
    /// Foreign-currency amount (`ご利用金額`). Optional, recorded in `source_meta`.
    #[serde(default)]
    pub use_amount: Option<String>,
    /// FX rate (`換算レート`). Optional, recorded in `source_meta`.
    #[serde(default)]
    pub rate: Option<String>,
}

/// How to match primary rows to debit rows.
#[derive(Debug, Clone, Deserialize)]
pub struct JoinConfig {
    /// ± days tolerated between the primary `posted_date` and the debit `date`.
    /// SBI typically posts a debit purchase on the merchant's settlement day
    /// (1-3 business days after the slip date), so a window of a few days
    /// catches the offset without false matches. Defaults to 7.
    #[serde(default = "default_date_window")]
    pub date_window_days: u32,
    /// GnuCash account name to use for the FX fee split. If `None`, the export
    /// falls back to `--default-other`.
    #[serde(default)]
    pub fx_fee_account: Option<String>,
    /// GnuCash account name for the per-transaction fee split. Optional.
    #[serde(default)]
    pub tx_fee_account: Option<String>,
    /// GnuCash account name for the ATM fee split. Optional.
    #[serde(default)]
    pub atm_fee_account: Option<String>,
}

impl Default for JoinConfig {
    fn default() -> Self {
        Self {
            date_window_days: default_date_window(),
            fx_fee_account: None,
            tx_fee_account: None,
            atm_fee_account: None,
        }
    }
}

fn default_date_window() -> u32 {
    7
}

fn default_encoding() -> String {
    "utf-8".into()
}
fn default_delimiter() -> String {
    ",".into()
}
fn default_header_row() -> usize {
    1
}
fn default_date_format() -> String {
    "%Y-%m-%d".into()
}

impl Profile {
    /// Parse a profile from TOML text. Validates the amount-column constraint.
    pub fn from_toml_str(text: &str) -> anyhow::Result<Self> {
        let p: Profile = toml::from_str(text)?;
        p.validate()?;
        Ok(p)
    }

    /// Read a profile from disk.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read profile {}: {e}", path.display()))?;
        Self::from_toml_str(&text)
    }

    /// Resolve `--profile <value>`:
    /// - if value ends in `.toml` or contains a path separator, treat as path
    /// - else look up `~/.config/fetchdoc/profiles/<value>.toml`
    pub fn resolve(value: &str) -> anyhow::Result<Self> {
        let looks_like_path =
            value.ends_with(".toml") || value.contains('/') || value.contains('\\');
        let path = if looks_like_path {
            PathBuf::from(value)
        } else {
            profile_dir()?.join(format!("{value}.toml"))
        };
        Self::from_path(&path)
    }

    /// Return the byte-level delimiter (`csv` crate wants a single byte).
    pub fn delimiter_byte(&self) -> anyhow::Result<u8> {
        delimiter_byte(&self.delimiter, &self.name)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let has_signed = self.columns.amount.is_some();
        let has_pair = self.columns.withdrawal.is_some() || self.columns.deposit.is_some();
        if has_signed && has_pair {
            anyhow::bail!(
                "profile {}: set either columns.amount OR columns.withdrawal/deposit, not both",
                self.name
            );
        }
        if !has_signed && !has_pair {
            anyhow::bail!(
                "profile {}: must set columns.amount or columns.withdrawal/deposit",
                self.name
            );
        }
        Ok(())
    }
}

impl DebitFile {
    pub fn delimiter_byte(&self) -> anyhow::Result<u8> {
        delimiter_byte(&self.delimiter, "multi.debit")
    }
}

fn delimiter_byte(s: &str, who: &str) -> anyhow::Result<u8> {
    match s.as_bytes() {
        [b] => Ok(*b),
        b"\\t" => Ok(b'\t'),
        _ => anyhow::bail!("{who}: delimiter must be a single byte, got {s:?}"),
    }
}

/// `~/.config/fetchdoc/profiles/` (respecting `$XDG_CONFIG_HOME`).
fn profile_dir() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("fetchdoc/profiles"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME not set; pass an explicit path to --profile"))?;
    Ok(PathBuf::from(home).join(".config/fetchdoc/profiles"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_signed_profile() {
        let toml = r#"
name = "test"
date_format = "%Y/%m/%d"

[columns]
posted_date = "Date"
description = "Memo"
amount = "Amount"
"#;
        let p = Profile::from_toml_str(toml).unwrap();
        assert_eq!(p.name, "test");
        assert_eq!(p.encoding, "utf-8");
        assert_eq!(p.delimiter_byte().unwrap(), b',');
        assert_eq!(p.columns.amount.as_deref(), Some("Amount"));
    }

    #[test]
    fn parse_withdrawal_deposit_profile() {
        let toml = r#"
name = "smbc"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "年月日"
description = "お取り扱い内容"
withdrawal = "お支払金額"
deposit = "お預り金額"
balance = "差引残高"
"#;
        let p = Profile::from_toml_str(toml).unwrap();
        assert_eq!(p.encoding, "shift_jis");
        assert_eq!(p.columns.withdrawal.as_deref(), Some("お支払金額"));
        assert_eq!(p.columns.balance.as_deref(), Some("差引残高"));
    }

    #[test]
    fn rejects_both_amount_and_withdrawal() {
        let toml = r#"
name = "bad"
[columns]
posted_date = "D"
description = "M"
amount = "A"
withdrawal = "W"
"#;
        assert!(Profile::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_missing_amount_columns() {
        let toml = r#"
name = "bad"
[columns]
posted_date = "D"
description = "M"
"#;
        assert!(Profile::from_toml_str(toml).is_err());
    }

    #[test]
    fn tab_delimiter_is_accepted() {
        let toml = r#"
name = "tsv"
delimiter = "\t"
[columns]
posted_date = "D"
description = "M"
amount = "A"
"#;
        let p = Profile::from_toml_str(toml).unwrap();
        assert_eq!(p.delimiter_byte().unwrap(), b'\t');
    }
}
