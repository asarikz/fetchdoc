//! GnuCash CSV export.
//!
//! Targets the GnuCash 4.x+ "Import Transactions from CSV" format. A row whose
//! `Date` is non-empty starts a new transaction; subsequent rows with empty
//! `Date` add additional splits to the previous transaction. We use this to
//! emit foreign debit purchases as principal + 海外事務手数料 on separate
//! GnuCash accounts. When `Transaction.splits` is `None` we fall back to the
//! single-row form (GnuCash auto-balances via `Transfer Account`).
//!
//! Reads [`Transaction`](crate::io::Transaction) JSONL on stdin (the output
//! of `import csv` ± `classify`). After writing the CSV, each record is
//! re-emitted on stdout with `exported.gnucash = ...` so further export
//! targets can chain.
//!
//! `Document` → GnuCash export (the invoice flow) lands once `classify` is
//! wired up. The current command only handles Transaction records.

use crate::io::{Split, Transaction, read_jsonl_stdin, write_jsonl_stdout};
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

    /// The GnuCash account that *is* this statement — e.g. the bank account
    /// the rows were imported from. Example: `Assets:Bank:SMBC`.
    #[arg(long)]
    pub account: String,

    /// Default offset account for transactions whose category is unknown.
    /// GnuCash will use this as the other split.
    #[arg(long, default_value = "Imbalance-JPY")]
    pub default_other: String,

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
    for rec in read_jsonl_stdin::<Transaction>() {
        let mut tx = rec.context("reading Transaction JSONL on stdin")?;
        write_row(&mut wtr, &tx, &args)?;
        tx.exported = Some(merge_exported(tx.exported.take(), &args.out));
        write_jsonl_stdout(&tx)?;
        written += 1;
    }
    wtr.flush()?;

    if !args.quiet {
        eprintln!("export gnucash: wrote {written} rows to {}", args.out);
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

fn write_row<W: Write>(
    wtr: &mut csv::Writer<W>,
    tx: &Transaction,
    args: &GnucashArgs,
) -> anyhow::Result<()> {
    let description = tx
        .counterparty_guess
        .clone()
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
            args.account.as_str(),
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
        args.account.as_str(),
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
