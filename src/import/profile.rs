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
        let bytes = self.delimiter.as_bytes();
        match bytes {
            [b] => Ok(*b),
            // accept the common escape `\t`
            b"\\t" => Ok(b'\t'),
            _ => anyhow::bail!(
                "profile {}: delimiter must be a single byte, got {:?}",
                self.name,
                self.delimiter
            ),
        }
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
