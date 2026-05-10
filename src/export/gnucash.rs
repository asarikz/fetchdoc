//! GnuCash CSV export.
//!
//! Targets GnuCash's **Transaction Export CSV** (the "gnc-trans" preset that
//! GnuCash's own `File → Export → Export Transactions to CSV` produces). One
//! split per row; rows belonging to the same transaction share the `Transaction
//! ID` column. The user re-imports it via `File → Import → Import Transactions
//! from CSV` and picks the bundled "gnc-trans" preset — no column mapping
//! needed.
//!
//! Reads either [`Transaction`](crate::io::Transaction) or
//! [`Document`](crate::io::Document) JSONL on stdin and dispatches per-record:
//!
//! - **Transaction** (bank/card statement row from `import csv` ± `classify`)
//!   → bank leg uses `--account`; the offsetting leg uses each split's
//!   `account` (or `category_guess`, or `--default-other`).
//! - **Document** (invoice PDF from `fetch ... | classify`) → standard accrual
//!   A/P pair: `--debit-account` for the expense, `--credit-account` for the
//!   payable.
//!
//! Every transaction is double-entry balanced (split amounts sum to zero).
//! After writing the CSV, each record is re-emitted on stdout with
//! `exported.gnucash = ...` so further export targets can chain.

use crate::export::accounts::{Chart, Pick, pick_debit_account};
use crate::io::{Document, Split, Transaction, write_jsonl_stdout};
use anyhow::Context;
use clap::Args;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;

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
    /// invoice). Example: `Expenses:諸経費`. When `--accounts` is also given,
    /// this becomes the *fallback* used only if the picker can't choose.
    #[arg(long)]
    pub debit_account: Option<String>,

    /// **Document input only.** Account to credit (the payable / cash side).
    /// Example: `Liabilities:買掛金` for accrual, `Assets:Cash` for cash purchases.
    #[arg(long)]
    pub credit_account: Option<String>,

    /// **Document input only.** Path to a GnuCash chart-of-accounts file (one
    /// fully-qualified account per line, `#` comments allowed). When set, the
    /// expense (debit) account is chosen per-document by an LLM call against
    /// this list. The chart goes in a cached system prompt so per-call cost
    /// is small. Falls back to `--debit-account` when the model can't pick.
    #[arg(long)]
    pub accounts: Option<PathBuf>,

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

    // Set up the LLM picker if --accounts was provided. We load the chart
    // up-front so a malformed file fails before the CSV gets written.
    let picker = match &args.accounts {
        Some(path) => {
            let chart = Chart::load(path)?;
            let client = crate::anthropic::Client::from_env().context(
                "--accounts requires Anthropic credentials for per-document account selection",
            )?;
            if !args.quiet {
                eprintln!(
                    "export gnucash: loaded {} accounts from {}",
                    chart.len(),
                    path.display()
                );
            }
            Some(PickerCtx { chart, client })
        }
        None => None,
    };

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
                let Some(extracted) = doc.extracted.clone() else {
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
                let credit = args.credit_account.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Document record requires --credit-account (the payable / cash side)"
                    )
                })?;

                // Decide the debit (expense) account: ask the LLM if --accounts
                // was given, otherwise use the fixed --debit-account.
                let (debit, debit_source, picker_note) = match &picker {
                    Some(ctx) => {
                        match pick_debit_account(&ctx.client, &ctx.chart, &doc, &extracted).await {
                            Ok(Pick::Picked(name)) => (name, "picker", None),
                            Ok(Pick::Fallback { reason }) => {
                                doc.status = "needs_review".to_string();
                                let fallback = args.debit_account.clone().ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "picker could not choose an account ({reason}) and \
                                     --debit-account fallback is not set"
                                    )
                                })?;
                                if !args.quiet {
                                    eprintln!(
                                        "export gnucash: {} → fallback {fallback} ({reason})",
                                        doc.external_id
                                    );
                                }
                                (fallback, "fallback", Some(reason))
                            }
                            Err(e) => {
                                // Network / API error — fall back too rather than
                                // aborting the whole batch, so a flaky call doesn't
                                // lose CSV rows we already wrote.
                                doc.status = "needs_review".to_string();
                                let fallback = args.debit_account.clone().ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "picker call failed ({e:#}) and --debit-account fallback \
                                     is not set"
                                    )
                                })?;
                                let reason = format!("picker error: {e:#}");
                                if !args.quiet {
                                    eprintln!(
                                        "export gnucash: {} → fallback {fallback} ({reason})",
                                        doc.external_id
                                    );
                                }
                                (fallback, "fallback", Some(reason))
                            }
                        }
                    }
                    None => {
                        let d = args.debit_account.clone().ok_or_else(|| {
                            anyhow::anyhow!(
                                "Document record requires --debit-account (the expense side) \
                                 or --accounts for picker mode"
                            )
                        })?;
                        (d, "fixed", None)
                    }
                };

                write_document_row(&mut wtr, &doc, &extracted, &debit, credit, &args)?;
                doc.exported = Some(merge_exported_doc(
                    doc.exported.take(),
                    &args.out,
                    &debit,
                    debit_source,
                    picker_note.as_deref(),
                ));
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

/// GnuCash transaction-export columns ("gnc-trans" preset). Order and labels
/// must match GnuCash's own export, otherwise the matching import preset
/// won't recognise the file.
const HEADER: &[&str] = &[
    "Date",
    "Transaction ID",
    "Number",
    "Description",
    "Notes",
    "Commodity/Currency",
    "Void Reason",
    "Action",
    "Memo",
    "Full Account Name",
    "Account Name",
    "Amount With Sym",
    "Amount Num.",
    "Value With Sym",
    "Value Num.",
    "Reconcile",
    "Reconcile Date",
    "Rate/Price",
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

    let date = format_date_us(&tx.posted_date)?;
    let txid = transaction_id_for(&tx.external_id);
    let notes = tx.memo.as_deref().unwrap_or("");

    // Build the split list. With explicit splits, the bank leg amount equals
    // the parent's signed amount and the rest comes from `tx.splits`. Without
    // splits, fabricate a two-leg transaction so the export is always balanced.
    let mut legs: Vec<Leg> = Vec::new();
    legs.push(Leg {
        account: account.to_string(),
        amount: tx.amount_jpy,
        memo: String::new(),
    });
    if let Some(splits) = tx.splits.as_ref() {
        validate_splits_balance(tx, splits)?;
        for split in splits {
            // Convention on Split: positive = money flowing INTO the offset
            // account when it's an expense booking. GnuCash's signed Amount
            // wants the same sign on the offset side, so pass through as-is.
            let acct = split
                .account
                .clone()
                .or_else(|| tx.category_guess.clone())
                .unwrap_or_else(|| args.default_other.clone());
            legs.push(Leg {
                account: acct,
                amount: split.amount_jpy,
                memo: split.note.clone().unwrap_or_default(),
            });
        }
    } else {
        // No explicit splits → fabricate the offset leg so the row pair is
        // self-balancing instead of relying on GnuCash's auto-balance.
        let other = tx
            .category_guess
            .clone()
            .unwrap_or_else(|| args.default_other.clone());
        legs.push(Leg {
            account: other,
            amount: -tx.amount_jpy,
            memo: String::new(),
        });
    }

    write_split_rows(
        wtr,
        &date,
        &txid,
        &description,
        notes,
        &legs,
        &args.currency,
    )
}

/// Emit a balanced two-leg transaction for an invoice/receipt: debit the
/// expense account, credit the payable (or cash). T number lands in the Notes
/// column where GnuCash makes it searchable.
fn write_document_row<W: Write>(
    wtr: &mut csv::Writer<W>,
    doc: &Document,
    extracted: &crate::io::Extracted,
    debit_account: &str,
    credit_account: &str,
    args: &GnucashArgs,
) -> anyhow::Result<()> {
    let date = format_date_us(&extracted.transaction_date)?;
    let txid = transaction_id_for(&doc.external_id);
    let notes = match extracted.counterparty_t_number.as_deref() {
        Some(t) => format!("T={t} ({})", doc.external_id),
        None => doc.external_id.clone(),
    };
    let amount = extracted.total_amount_jpy;
    let legs = [
        Leg {
            account: debit_account.to_string(),
            amount, // Debit expense (positive)
            memo: String::new(),
        },
        Leg {
            account: credit_account.to_string(),
            amount: -amount, // Credit payable / cash (negative)
            memo: String::new(),
        },
    ];
    write_split_rows(
        wtr,
        &date,
        &txid,
        &extracted.counterparty_name,
        &notes,
        &legs,
        &args.currency,
    )
}

/// One split row in a balanced GnuCash transaction.
struct Leg {
    account: String,
    amount: i64,
    memo: String,
}

/// Emit one CSV row per leg, all sharing the same Date / Transaction ID /
/// Description / Notes — that's how GnuCash's import wizard groups rows back
/// into a single multi-split transaction.
fn write_split_rows<W: Write>(
    wtr: &mut csv::Writer<W>,
    date: &str,
    txid: &str,
    description: &str,
    notes: &str,
    legs: &[Leg],
    currency: &str,
) -> anyhow::Result<()> {
    let commodity = format!("CURRENCY::{currency}");
    for leg in legs {
        let (with_sym, num) = format_amount(leg.amount, currency);
        let leaf = leaf_account(&leg.account);
        wtr.write_record([
            date,
            txid,
            "", // Number
            description,
            notes,
            commodity.as_str(),
            "", // Void Reason
            "", // Action
            leg.memo.as_str(),
            leg.account.as_str(),
            leaf,
            with_sym.as_str(),
            num.as_str(),
            with_sym.as_str(), // Value mirrors Amount in single-currency mode
            num.as_str(),
            "n", // Reconcile = not reconciled
            "",  // Reconcile Date
            "1.00",
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

/// Document-flavoured `exported.gnucash` payload: includes which debit account
/// was used and how it was chosen (picker / fallback / fixed) so the user can
/// audit picker accuracy without re-running the export.
fn merge_exported_doc(
    prev: Option<serde_json::Value>,
    out_path: &str,
    debit_account: &str,
    debit_source: &str,
    picker_note: Option<&str>,
) -> serde_json::Value {
    let mut obj = match prev {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    let mut payload = json!({
        "out": out_path,
        "debit_account": debit_account,
        "debit_source": debit_source,
    });
    if let Some(note) = picker_note {
        payload
            .as_object_mut()
            .expect("payload is an object literal")
            .insert("picker_note".into(), serde_json::Value::String(note.into()));
    }
    obj.insert("gnucash".into(), payload);
    serde_json::Value::Object(obj)
}

/// Borrowed bundle of picker state held by `run` for the duration of the batch.
struct PickerCtx {
    chart: Chart,
    client: crate::anthropic::Client,
}

/// Convert `YYYY-MM-DD` (our canonical form) to `MM/DD/YYYY` (GnuCash's export
/// default). The import preset reads the date format from a sibling sidecar
/// when available; emitting US-style keeps round-tripping with GnuCash's own
/// export.
fn format_date_us(iso: &str) -> anyhow::Result<String> {
    let bytes = iso.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        anyhow::bail!("expected YYYY-MM-DD date, got {iso:?}");
    }
    Ok(format!("{}/{}/{}", &iso[5..7], &iso[8..10], &iso[0..4]))
}

/// Last segment of a colon-delimited GnuCash full account name.
/// `Assets:Bank:GMO` → `GMO`. Empty input → empty.
fn leaf_account(full: &str) -> &str {
    full.rsplit(':').next().unwrap_or(full)
}

/// Format a signed integer amount as the `(Amount With Sym, Amount Num.)` pair
/// GnuCash writes. JPY (and other zero-decimal currencies) get integer output
/// with `JP¥` symbol; anything else falls back to `<CODE> <number>`.
fn format_amount(amount: i64, currency: &str) -> (String, String) {
    let num = amount.to_string();
    let symbol = currency_symbol(currency);
    let abs = amount.unsigned_abs();
    let with_sep = thousands(abs);
    let with_sym = if amount < 0 {
        format!("-{symbol}{with_sep}")
    } else {
        format!("{symbol}{with_sep}")
    };
    (with_sym, num)
}

fn currency_symbol(code: &str) -> &'static str {
    match code {
        "JPY" => "JP¥",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        // Fallback: empty prefix; the Commodity/Currency column still
        // disambiguates on import.
        _ => "",
    }
}

/// Comma-grouped thousands. `2700000` → `2,700,000`. ASCII-only input.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Stable 32-hex Transaction ID derived from the source's external id. Same
/// shape as GnuCash's own GUIDs (32 hex chars, no dashes). Re-running the
/// export on the same record produces the same id, which makes idempotent
/// re-imports possible if the user wires that up.
fn transaction_id_for(external_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(external_id.as_bytes());
    let mut hex = String::with_capacity(32);
    for b in &digest[..16] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}
