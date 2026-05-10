#![allow(dead_code)] // Helpers are wired in subsequent feature PRs.

//! Shared JSON Lines I/O helpers.
//!
//! Every subcommand follows the same convention:
//! - **stdin**: one JSON object per line (`Document`)
//! - **stdout**: one JSON object per line (`Document` with additional fields)
//! - **stderr**: human-readable progress (suppressed by `--quiet`)
//!
//! Records flow through the pipeline accumulating fields. `fetch` produces
//! the initial record; `classify` adds `extracted`; `export` adds `exported`.

use serde::{Deserialize, Serialize};

/// One row of the fetchdoc pipeline. Fields accumulate as the document
/// passes through subcommands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Upstream system (`"gmail"`, `"outlook"`, `"local"`).
    pub source: String,
    /// Stable upstream id — Gmail messageId, Outlook itemId, etc.
    pub external_id: String,
    /// Path to the local copy of the attachment (set by `fetch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_path: Option<String>,
    /// Raw subject / sender / date — anything `fetch` knows about the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_meta: Option<serde_json::Value>,
    /// Structured extraction (set by `classify`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<Extracted>,
    /// Result of a successful export step (set by `export`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported: Option<serde_json::Value>,
    /// `"ok"` (default) or `"needs_review"` — set by `classify` when validation fails.
    #[serde(default = "Document::default_status")]
    pub status: String,
}

impl Document {
    fn default_status() -> String {
        "ok".to_string()
    }
}

/// Structured fields extracted from the document by `classify`.
///
/// Mirrors Japanese qualified-invoice (適格請求書) requirements:
/// transaction date, total amount, counterparty name, optional T number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extracted {
    /// ISO date string (`YYYY-MM-DD`) of the transaction.
    pub transaction_date: String,
    /// Total amount in JPY (or other currency unit if non-JPY documents are added later).
    pub total_amount_jpy: i64,
    /// Counterparty name as it appears on the invoice.
    pub counterparty_name: String,
    /// Qualified-invoice registration number, if present (`T` + 13 digits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty_t_number: Option<String>,
    /// Self-reported confidence from the OCR backend (0.0 .. 1.0).
    pub confidence: f64,
}

/// Read JSONL one line at a time from a buffered reader, deserialising each
/// line into a `Document`. Empty lines are skipped.
pub fn read_jsonl_stdin() -> impl Iterator<Item = anyhow::Result<Document>> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let lock = stdin.lock();
    lock.lines().filter_map(|line_res| match line_res {
        Ok(line) if line.trim().is_empty() => None,
        Ok(line) => Some(serde_json::from_str(&line).map_err(anyhow::Error::from)),
        Err(e) => Some(Err(e.into())),
    })
}

/// Write a single record as one JSONL line to stdout, flushing immediately so
/// downstream commands in a pipe see records as they're produced.
pub fn write_jsonl_stdout(doc: &Document) -> anyhow::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, doc)?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}
