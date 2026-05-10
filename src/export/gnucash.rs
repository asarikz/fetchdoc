//! GnuCash CSV export.
//!
//! Targets the GnuCash 4.x+ "Import Transactions from CSV" format. A row whose
//! `Date` is non-empty starts a new transaction; subsequent rows with empty
//! `Date` add additional splits to the previous transaction. We use this to
//! emit foreign debit purchases as principal + 海外事務手数料 on separate
//! GnuCash accounts. When `Transaction.splits` is `None` we fall back to the
//! single-row form (GnuCash auto-balances via `Transfer Account`).
//!
//! Reads either [`Transaction`](crate::io::Transaction) or
//! [`Document`](crate::io::Document) JSONL on stdin and dispatches per-record:
//!
//! - **Transaction** (bank/card statement row from `import csv` ± `classify`)
//!   → uses `--account` as the bank leg, `--default-other` (or
//!   `category_guess`) as the offset.
//! - **Document** (invoice PDF from `fetch ... | classify`) → emits the
//!   classical accrual A/P pair: `--debit-account` for the expense,
//!   `--credit-account` for the payable. The CSV row uses GnuCash's
//!   single-split form (Account / Deposit / Transfer Account).
//!
//! After writing the CSV, each record is re-emitted on stdout with
//! `exported.gnucash = ...` so further export targets can chain.

use crate::io::{Document, Split, Transaction, write_jsonl_stdout};
use anyhow::Context;
use clap::Args;
use serde_json::json;
use std::io::Write;

#[derive(Args, Debug)]
pub struct GnucashArgs {
    /// Output CSV path. Stdout is reserved for the JSONL passthrough, so a
    /// file path is required here.
    #[arg(long)]
    pub out: String,

    /// **Transaction input only.** The GnuCash account that *is* this
    /// statement — e.g. the bank account the rows were imported from.
    /// Example: `Assets:Bank:SMBC`.
    #[arg(long)]
    pub account: Option<String>,

    /// **Transaction input only.** Default offset account for transactions
    /// whose category is unknown. GnuCash will use this as the other split.
    #[arg(long, default_value = "Imbalance-JPY")]
    pub default_other: String,

    /// **Document input only.** Account to debit (the expense side of the
    /// invoice). Example: `Expenses:諸経費`.
    #[arg(long)]
    pub debit_account: Option<String>,

    /// **Document input only.** Account to credit (the payable / cash side).
    /// Example: `Liabilities:買掛金` for accrual, `Assets:Cash` for cash purchases.
    #[arg(long)]
    pub credit_account: Option<String>,

    /// Currency commodity code. Defaults to JPY.
    #[arg(long, default_value = "JPY")]
    pub currency: String,

    /// Suppress per-row stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: GnucashArgs) -> anyhow::Result<()> {
    if args.out == "-" {
        anyhow::bail!("--out '-' (stdout) conflicts with the JSONL passthrough; pass a file path");
    }

    let file =
        std::fs::File::create(&args.out).with_context(|| format!("creating {}", args.out))?;
    let mut wtr = csv::Writer::from_writer(std::io::BufWriter::new(file));
    wtr.write_record(HEADER)?;

    let mut written = 0usize;
    let mut skipped = 0usize;
    for line_res in read_jsonl_lines() {
        let line = line_res.context("reading JSONL on stdin")?;
        let value: serde_json::Value =
            serde_json::from_str(&line).context("parsing JSONL record")?;

        match classify_record(&value) {
            RecordKind::Transaction => {
                let mut tx: Transaction =
                    serde_json::from_value(value).context("decoding record as Transaction")?;
                let account = args.account.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Transaction record requires --account (the bank-statement account)"
                    )
                })?;
                write_transaction_row(&mut wtr, &tx, account, &args)?;
                tx.exported = Some(merge_exported(tx.exported.take(), &args.out));
                write_jsonl_stdout(&tx)?;
                written += 1;
            }
            RecordKind::Document => {
                let mut doc: Document =
                    serde_json::from_value(value).context("decoding record as Document")?;
                let Some(extracted) = doc.extracted.as_ref() else {
                    doc.status = "needs_review".to_string();
                    skipped += 1;
                    if !args.quiet {
                        eprintln!(
                            "export gnucash: skipped {} (no extracted fields; run classify first)",
                            doc.external_id
                        );
                    }
                    write_jsonl_stdout(&doc)?;
                    continue;
                };
                let debit = args.debit_account.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("Document record requires --debit-account (the expense side)")
                })?;
                let credit = args.credit_account.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Document record requires --credit-account (the payable / cash side)"
                    )
                })?;
                write_document_row(&mut wtr, &doc, extracted, debit, credit, &args)?;
                doc.exported = Some(merge_exported(doc.exported.take(), &args.out));
                write_jsonl_stdout(&doc)?;
                written += 1;
            }
            RecordKind::Unknown => {
                anyhow::bail!(
                    "record is neither Transaction (no `posted_date`) nor Document \
                     (no `attachment_path`/`extracted`); cannot export"
                );
            }
        }
    }
    wtr.flush()?;

    if !args.quiet {
        eprintln!(
            "export gnucash: wrote {written} rows to {}{}",
            args.out,
            if skipped > 0 {
                format!(" ({skipped} skipped)")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// GnuCash 4.x importer columns. Field names match the labels GnuCash shows
/// in the CSV import wizard, so the user can map them by name in one click.
const HEADER: &[&str] = &[
    "Date",
    "Description",
    "Notes",
    "Account",
    "Deposit",
    "Withdrawal",
    "Transfer Account",
    "Commodity/Currency",
];

enum RecordKind {
    Transaction,
    Document,
    Unknown,
}

/// Decide whether a JSON object looks like a `Transaction` or a `Document`.
/// Both shapes share `external_id` / `source` / `status`, so we key off the
/// fields that are unique to each: `posted_date` (Transaction, required) and
/// `attachment_path`/`extracted` (Document).
fn classify_record(v: &serde_json::Value) -> RecordKind {
    if v.get("posted_date").is_some() || v.get("amount_jpy").is_some() {
        RecordKind::Transaction
    } else if v.get("attachment_path").is_some() || v.get("extracted").is_some() {
        RecordKind::Document
    } else {
        RecordKind::Unknown
    }
}

/// Same line-at-a-time iterator as `read_jsonl_stdin`, but yields raw strings
/// so we can peek at fields before committing to a concrete type.
fn read_jsonl_lines() -> impl Iterator<Item = std::io::Result<String>> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let lock = stdin.lock();
    lock.lines()
        .filter(|res| res.as_ref().map(|l| !l.trim().is_empty()).unwrap_or(true))
}

fn write_transaction_row<W: Write>(
    wtr: &mut csv::Writer<W>,
    tx: &Transaction,
    account: &str,
    args: &GnucashArgs,
) -> anyhow::Result<()> {
    let description = tx
        .counterparty_guess
        .clone()
        .or_else(|| tx.description_normalized.clone())
        .unwrap_or_else(|| tx.description_raw.clone());

    // No explicit splits → emit the legacy single-row form so GnuCash's
    // single-split importer mode still works for the common case.
    let Some(splits) = tx.splits.as_ref() else {
        let (deposit, withdrawal) = if tx.amount_jpy >= 0 {
            (tx.amount_jpy.to_string(), String::new())
        } else {
            (String::new(), (-tx.amount_jpy).to_string())
        };
        let other = tx
            .category_guess
            .clone()
            .unwrap_or_else(|| args.default_other.clone());
        wtr.write_record([
            tx.posted_date.as_str(),
            description.as_str(),
            tx.memo.as_deref().unwrap_or(""),
            account,
            deposit.as_str(),
            withdrawal.as_str(),
            other.as_str(),
            args.currency.as_str(),
        ])?;
        return Ok(());
    };

    validate_splits_balance(tx, splits)?;

    // Multi-split: row 1 carries the bank leg + transaction header (Date,
    // Description, Notes); rows 2..N each add one split with empty
    // Date/Description so GnuCash treats them as continuations.
    let (bank_dep, bank_wd) = if tx.amount_jpy >= 0 {
        (tx.amount_jpy.to_string(), String::new())
    } else {
        (String::new(), (-tx.amount_jpy).to_string())
    };
    wtr.write_record([
        tx.posted_date.as_str(),
        description.as_str(),
        tx.memo.as_deref().unwrap_or(""),
        account,
        bank_dep.as_str(),
        bank_wd.as_str(),
        "", // Transfer Account unused in multi-split mode
        args.currency.as_str(),
    ])?;

    for split in splits {
        // Sign convention: positive `amount_jpy` on a Split = expense (money
        // flowing INTO the offsetting account = Deposit on its books).
        let (split_dep, split_wd) = if split.amount_jpy >= 0 {
            (split.amount_jpy.to_string(), String::new())
        } else {
            (String::new(), (-split.amount_jpy).to_string())
        };
        let acct = split
            .account
            .clone()
            .or_else(|| tx.category_guess.clone())
            .unwrap_or_else(|| args.default_other.clone());
        wtr.write_record([
            "", // Date empty → continuation row
            "", // Description belongs to the parent
            split.note.as_deref().unwrap_or(""),
            acct.as_str(),
            split_dep.as_str(),
            split_wd.as_str(),
            "",
            args.currency.as_str(),
        ])?;
    }
    Ok(())
}

/// Emit one GnuCash row for an invoice/receipt: debit the expense account,
/// credit the payable (or cash). T number lands in the Notes column where
/// GnuCash makes it searchable.
fn write_document_row<W: Write>(
    wtr: &mut csv::Writer<W>,
    doc: &Document,
    extracted: &crate::io::Extracted,
    debit_account: &str,
    credit_account: &str,
    args: &GnucashArgs,
) -> anyhow::Result<()> {
    let amount = extracted.total_amount_jpy.to_string();
    let notes = match extracted.counterparty_t_number.as_deref() {
        Some(t) => format!("T={t} ({})", doc.external_id),
        None => doc.external_id.clone(),
    };
    wtr.write_record([
        extracted.transaction_date.as_str(),
        extracted.counterparty_name.as_str(),
        notes.as_str(),
        debit_account,
        amount.as_str(), // Deposit on the debit-account side = expense recognised
        "",
        credit_account,
        args.currency.as_str(),
    ])?;
    Ok(())
}

/// Multi-split invariant: the offsetting splits must net out the bank leg.
/// `amount_jpy` on the parent is the bank account's signed delta; the splits
/// (positive = expense out of bank) must therefore sum to `-amount_jpy`. A
/// drift here would silently produce an imbalanced transaction in GnuCash.
fn validate_splits_balance(tx: &Transaction, splits: &[Split]) -> anyhow::Result<()> {
    let sum: i64 = splits.iter().map(|s| s.amount_jpy).sum();
    if sum != -tx.amount_jpy {
        anyhow::bail!(
            "splits do not balance for {}: sum={sum}, expected={}; check the importer",
            tx.external_id,
            -tx.amount_jpy
        );
    }
    Ok(())
}

fn merge_exported(prev: Option<serde_json::Value>, out_path: &str) -> serde_json::Value {
    let mut obj = match prev {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("gnucash".into(), json!({ "out": out_path }));
    serde_json::Value::Object(obj)
}
