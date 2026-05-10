#![allow(dead_code)] // Helpers are wired in subsequent feature PRs.

//! Shared JSON Lines I/O helpers.
//!
//! Every subcommand follows the same convention:
//! - **stdin**: one JSON object per line
//! - **stdout**: one JSON object per line (often the same record with extra fields)
//! - **stderr**: human-readable progress (suppressed by `--quiet`)
//!
//! Two record shapes flow through the pipeline:
//! - [`Document`] — invoice/receipt PDFs (Gmail → classify → export)
//! - [`Transaction`] — bank/card line items (CSV/xlsx import → classify → export)
//!
//! Records accumulate fields as they pass through subcommands.

use serde::{Serialize, de::DeserializeOwned};

/// One row of the fetchdoc invoice pipeline. Fields accumulate as the document
/// passes through subcommands.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(default = "default_status")]
    pub status: String,
}

/// Structured fields extracted from the document by `classify`.
///
/// Mirrors Japanese qualified-invoice (適格請求書) requirements:
/// transaction date, total amount, counterparty name, optional T number.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Document type — invoice (請求書), receipt (領収書), or other.
    /// Lets `export local` filename templates distinguish 請求書/領収書, and
    /// lets `export gnucash` collapse the common Amazon-style invoice+receipt
    /// pair so a single transaction is not booked twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type: Option<DocumentType>,
    /// Self-reported confidence from the OCR backend (0.0 .. 1.0).
    pub confidence: f64,
}

/// Whether the document is an invoice (請求書 — billing document, sent before
/// payment), a receipt (領収書 — proof of payment, sent after), or something
/// else (delivery note, statement, etc.). Used by `export local` for
/// filename templating and by `export gnucash` for invoice/receipt dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentType {
    /// 請求書 — billing document.
    Invoice,
    /// 領収書 — proof of payment.
    Receipt,
    /// Anything else (delivery note 納品書, statement 明細書, …).
    Other,
}

impl DocumentType {
    /// Japanese label used by the `{document_type_ja}` filename placeholder.
    pub fn ja(self) -> &'static str {
        match self {
            Self::Invoice => "請求書",
            Self::Receipt => "領収書",
            Self::Other => "その他",
        }
    }

    /// English label (matches the lowercase serde tag) used by
    /// `{document_type}`.
    pub fn en(self) -> &'static str {
        match self {
            Self::Invoice => "invoice",
            Self::Receipt => "receipt",
            Self::Other => "other",
        }
    }
}

/// One row of the bank/card transaction pipeline. Produced by `import csv`
/// and `import xlsx`; consumed by `classify` (counterparty/category guesses)
/// and `export gnucash`.
///
/// Sign convention: `amount_jpy` is **signed** — outflows (withdrawals) are
/// negative, inflows (deposits) positive. This matches GnuCash's transfer
/// semantics and lets a single field replace the typical Japanese-bank
/// `withdrawal` / `deposit` column pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Transaction {
    /// Upstream type — `"csv"`, `"xlsx"`, etc.
    pub source: String,
    /// Profile name used to parse the source, e.g. `"smbc"`. `None` if the
    /// caller declined a profile (raw passthrough).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_profile: Option<String>,
    /// Stable id derived from profile + date + amount + description. Used for
    /// deduplication on re-import.
    pub external_id: String,
    /// Posted/booking date (`YYYY-MM-DD`).
    pub posted_date: String,
    /// Value date if the source distinguishes it from posted date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_date: Option<String>,
    /// Signed amount in JPY. Outflows negative, inflows positive.
    pub amount_jpy: i64,
    /// Running balance after this transaction, if the source provides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_jpy: Option<i64>,
    /// Description as it appears in the source, before any normalisation.
    pub description_raw: String,
    /// Cleaned-up description (half-width katakana → full-width, etc.).
    /// `None` when normalisation produced no change or was disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_normalized: Option<String>,
    /// Counterparty guess from `classify`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty_guess: Option<String>,
    /// Free-form memo column from the source, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    /// GnuCash-style account suggestion from `classify` (e.g. `Expenses:Food`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_guess: Option<String>,
    /// Optional explicit splits when the source row maps to a GnuCash multi-split
    /// transaction (e.g. SBI Sumishin debit: principal + 海外事務手数料 separated).
    /// When set, the sum of `amount_jpy` across splits must equal `-amount_jpy`
    /// of the parent (i.e. the bank-account leg). When unset, `export gnucash`
    /// emits a single-row transaction using `category_guess` as the offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub splits: Option<Vec<Split>>,
    /// Anything else the importer wants to keep around (raw row, file path, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_meta: Option<serde_json::Value>,
    /// Result of a successful export step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported: Option<serde_json::Value>,
    /// `"ok"` (default) or `"needs_review"`.
    #[serde(default = "default_status")]
    pub status: String,
}

/// One leg of a GnuCash multi-split transaction (the offsetting side, viewed
/// from the bank account's perspective). Positive `amount_jpy` is an outflow
/// to `account` (the typical case for a debit-card purchase split between
/// principal and FX fee); negative is a refund.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Split {
    /// Target GnuCash account, e.g. `Expenses:支払手数料:海外事務手数料`.
    /// `None` defers to `export gnucash --default-other`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Amount routed to `account`, in JPY. Positive = expense, negative = refund.
    pub amount_jpy: i64,
    /// Optional per-split note (becomes the GnuCash split memo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_status() -> String {
    "ok".to_string()
}

/// Read JSONL one line at a time from stdin, deserialising each line as `T`.
/// Empty lines are skipped.
pub fn read_jsonl_stdin<T: DeserializeOwned>() -> impl Iterator<Item = anyhow::Result<T>> {
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
pub fn write_jsonl_stdout<T: Serialize>(record: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, record)?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}
