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
