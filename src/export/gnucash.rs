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

use crate::export::accounts::{Chart, Pick, pick_debit_account};
use crate::io::{Document, DocumentType, Split, Transaction, write_jsonl_stdout};
use anyhow::Context;
use clap::Args;
use serde_json::json;
use std::collections::HashMap;
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

    /// **Document input only.** Disable invoice/receipt deduplication. By
    /// default, Documents that share `(transaction_date, total_amount_jpy,
    /// counterparty_name)` are collapsed to one CSV row to avoid double-
    /// counting Amazon-style invoice + receipt pairs (the receipt — 領収書 —
    /// is preferred). Pass this flag to write a row per Document instead.
    #[arg(long, default_value_t = false)]
    pub keep_duplicates: bool,

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
    let mut suppressed = 0usize;

    // Documents are buffered so we can dedup invoice/receipt pairs across the
    // whole input batch (one transaction must produce only one GnuCash row).
    // Transactions still stream so the bank-statement pipeline keeps its
    // line-at-a-time behaviour — no need to dedup a statement against itself.
    let mut buffered_docs: Vec<Document> = Vec::new();

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
                let doc: Document =
                    serde_json::from_value(value).context("decoding record as Document")?;
                buffered_docs.push(doc);
            }
            RecordKind::Unknown => {
                anyhow::bail!(
                    "record is neither Transaction (no `posted_date`) nor Document \
                     (no `attachment_path`/`extracted`); cannot export"
                );
            }
        }
    }

    // Split buffered documents into "kept" (one row each in CSV) and
    // "suppressed" (duplicates that get a JSONL passthrough but no CSV row).
    // Records missing `extracted` always end up "kept" so they keep their
    // existing needs_review behaviour rather than getting silently dropped.
    let DedupResult {
        kept,
        suppressed: suppressed_docs,
    } = if args.keep_duplicates {
        DedupResult {
            kept: buffered_docs,
            suppressed: Vec::new(),
        }
    } else {
        dedup_documents(buffered_docs, args.quiet)
    };

    for mut doc in kept {
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
            anyhow::anyhow!("Document record requires --credit-account (the payable / cash side)")
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

    // Suppressed duplicates: pass through with a marker so downstream tools
    // (or the user's eye on the JSONL log) can see *why* they did not become
    // a CSV row. status stays "ok" because skipping is intentional, not an
    // error — using needs_review here would be misleading.
    for mut doc in suppressed_docs {
        doc.exported = Some(merge_exported_doc_suppressed(
            doc.exported.take(),
            &args.out,
        ));
        write_jsonl_stdout(&doc)?;
        suppressed += 1;
    }

    wtr.flush()?;

    if !args.quiet {
        let mut tail = String::new();
        if skipped > 0 {
            tail.push_str(&format!(" ({skipped} skipped"));
            if suppressed > 0 {
                tail.push_str(&format!(", {suppressed} duplicates suppressed"));
            }
            tail.push(')');
        } else if suppressed > 0 {
            tail.push_str(&format!(" ({suppressed} duplicates suppressed)"));
        }
        eprintln!("export gnucash: wrote {written} rows to {}{tail}", args.out);
    }
    Ok(())
}

/// Outcome of the invoice/receipt dedup pass.
struct DedupResult {
    /// Documents that should each produce a CSV row (one per real-world
    /// transaction). Missing-`extracted` records pass through here so they
    /// keep their existing needs_review path.
    kept: Vec<Document>,
    /// Duplicate Documents that were collapsed away from the CSV. They still
    /// flow on stdout so callers can audit them.
    suppressed: Vec<Document>,
}

/// Group documents by `(transaction_date, total_amount_jpy, counterparty_name)`
/// and pick one per group. Preference order within a group:
/// receipt (領収書) > invoice (請求書) > other > unknown. Ties (same priority)
/// are broken by `external_id` lexicographic order so the choice is
/// deterministic across runs.
///
/// Documents without `extracted` are never grouped — we have nothing reliable
/// to compare on, so they pass through unchanged and the existing missing-
/// `extracted` path downstream marks them needs_review.
fn dedup_documents(docs: Vec<Document>, quiet: bool) -> DedupResult {
    // Preserve original input order for the kept-list. We bucket by group
    // key and remember the index of the first record in each group, so the
    // final output order matches the order in which each group's first
    // record appeared in stdin.
    let mut groups: HashMap<DedupKey, Vec<usize>> = HashMap::new();
    let mut group_first_idx: HashMap<DedupKey, usize> = HashMap::new();
    let mut ungrouped: Vec<(usize, Document)> = Vec::new();
    let mut grouped: Vec<(usize, Document)> = Vec::new();

    for (idx, doc) in docs.into_iter().enumerate() {
        match dedup_key(&doc) {
            Some(key) => {
                group_first_idx.entry(key.clone()).or_insert(idx);
                groups.entry(key.clone()).or_default().push(grouped.len());
                grouped.push((idx, doc));
            }
            None => ungrouped.push((idx, doc)),
        }
    }

    let mut kept_with_idx: Vec<(usize, Document)> = ungrouped;
    let mut suppressed: Vec<Document> = Vec::new();

    for (key, mut indices) in groups {
        // Sort the records in this group by preference, ascending: index 0
        // becomes the kept record.
        indices.sort_by(|&a, &b| {
            let da = &grouped[a].1;
            let db = &grouped[b].1;
            doc_priority(da)
                .cmp(&doc_priority(db))
                .then_with(|| da.external_id.cmp(&db.external_id))
        });
        let kept_local = indices[0];
        let kept_doc = &grouped[kept_local].1;
        let first_idx = *group_first_idx.get(&key).expect("group has first idx");
        let kept_external_id = kept_doc.external_id.clone();
        let kept_type_label = type_label(kept_doc);
        if indices.len() > 1 && !quiet {
            let losers: Vec<String> = indices[1..]
                .iter()
                .map(|&i| {
                    format!(
                        "{} ({})",
                        grouped[i].1.external_id,
                        type_label(&grouped[i].1),
                    )
                })
                .collect();
            eprintln!(
                "export gnucash: dedup [{} / ¥{} / {}]: kept {} ({}), suppressed {}",
                key.date,
                key.amount,
                key.counterparty,
                kept_external_id,
                kept_type_label,
                losers.join(", "),
            );
        }
        // Push the kept record using the group's first stdin index so the
        // final order respects the order in which the user fed records in.
        kept_with_idx.push((first_idx, grouped[kept_local].1.clone()));
        for &i in &indices[1..] {
            suppressed.push(grouped[i].1.clone());
        }
    }

    kept_with_idx.sort_by_key(|(i, _)| *i);
    let kept = kept_with_idx.into_iter().map(|(_, d)| d).collect();
    DedupResult { kept, suppressed }
}

/// The grouping key used to detect "this is the same transaction described by
/// two different documents" — the Amazon invoice/receipt case. Counterparty
/// names that differ between the two will *not* be grouped; that's a
/// deliberate trade-off (we'd rather leave a duplicate than collapse two
/// genuinely distinct purchases).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    date: String,
    amount: i64,
    counterparty: String,
}

fn dedup_key(doc: &Document) -> Option<DedupKey> {
    let ex = doc.extracted.as_ref()?;
    Some(DedupKey {
        date: ex.transaction_date.clone(),
        amount: ex.total_amount_jpy,
        counterparty: ex.counterparty_name.clone(),
    })
}

/// Smaller is more preferred. Within a duplicate group we keep the lowest.
fn doc_priority(doc: &Document) -> u8 {
    match doc.extracted.as_ref().and_then(|e| e.document_type) {
        Some(DocumentType::Receipt) => 0,
        Some(DocumentType::Invoice) => 1,
        Some(DocumentType::Other) => 2,
        None => 3,
    }
}

fn type_label(doc: &Document) -> &'static str {
    match doc.extracted.as_ref().and_then(|e| e.document_type) {
        Some(t) => t.en(),
        None => "unknown",
    }
}

fn merge_exported_doc_suppressed(
    prev: Option<serde_json::Value>,
    out_path: &str,
) -> serde_json::Value {
    let mut obj = match prev {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(
        "gnucash".into(),
        json!({
            "out": out_path,
            "suppressed_as_duplicate": true,
        }),
    );
    serde_json::Value::Object(obj)
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

#[cfg(test)]
mod dedup_tests {
    use super::*;
    use crate::io::Extracted;

    fn doc(external_id: &str, dt: Option<DocumentType>, name: &str) -> Document {
        Document {
            source: "local".into(),
            external_id: external_id.into(),
            attachment_path: Some(format!("/p/{external_id}.pdf")),
            source_meta: None,
            extracted: Some(Extracted {
                transaction_date: "2026-04-30".into(),
                total_amount_jpy: 1980,
                counterparty_name: name.into(),
                counterparty_t_number: None,
                document_type: dt,
                confidence: 0.9,
            }),
            exported: None,
            status: "ok".into(),
        }
    }

    #[test]
    fn invoice_and_receipt_collapse_to_receipt() {
        let res = dedup_documents(
            vec![
                doc("inv", Some(DocumentType::Invoice), "Amazon"),
                doc("rcp", Some(DocumentType::Receipt), "Amazon"),
            ],
            true,
        );
        assert_eq!(res.kept.len(), 1);
        assert_eq!(res.kept[0].external_id, "rcp");
        assert_eq!(res.suppressed.len(), 1);
        assert_eq!(res.suppressed[0].external_id, "inv");
    }

    #[test]
    fn distinct_counterparty_keeps_both() {
        let res = dedup_documents(
            vec![
                doc("a", Some(DocumentType::Receipt), "Amazon"),
                doc("b", Some(DocumentType::Receipt), "ヨドバシ"),
            ],
            true,
        );
        assert_eq!(res.kept.len(), 2);
        assert!(res.suppressed.is_empty());
    }

    #[test]
    fn missing_extracted_passes_through_kept() {
        // No `extracted` → can't dedup, must pass through to the
        // existing needs_review path.
        let mut d = doc("no-classify", None, "X");
        d.extracted = None;
        let res = dedup_documents(vec![d], true);
        assert_eq!(res.kept.len(), 1);
        assert_eq!(res.kept[0].external_id, "no-classify");
    }

    #[test]
    fn unknown_type_loses_to_typed() {
        // Three records: one untyped, one invoice, one receipt → keep receipt.
        let res = dedup_documents(
            vec![
                doc("u", None, "Amazon"),
                doc("i", Some(DocumentType::Invoice), "Amazon"),
                doc("r", Some(DocumentType::Receipt), "Amazon"),
            ],
            true,
        );
        assert_eq!(res.kept.len(), 1);
        assert_eq!(res.kept[0].external_id, "r");
        assert_eq!(res.suppressed.len(), 2);
    }

    #[test]
    fn ties_break_deterministically_by_external_id() {
        let res = dedup_documents(
            vec![
                doc("zzz", Some(DocumentType::Receipt), "Amazon"),
                doc("aaa", Some(DocumentType::Receipt), "Amazon"),
            ],
            true,
        );
        assert_eq!(res.kept.len(), 1);
        assert_eq!(res.kept[0].external_id, "aaa");
    }
}
