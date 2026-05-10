//! Anthropic Claude OCR/extraction backend for `classify`.
//!
//! Reads `Document` JSONL on stdin. For each document, sends the PDF at
//! `attachment_path` to the Messages API as a `document` content block and
//! asks Claude to return a small JSON object with the qualified-invoice
//! fields. Successful extractions are written back as `extracted`; failures
//! flip `status` to `needs_review` so the rest of the pipeline can still
//! process them.
//!
//! One PDF per request — keeps prompts small, lets a partial failure stay
//! local, and matches how downstream consumers iterate.

use crate::anthropic::{Client, UserBlock};
use crate::classify::ClassifyArgs;
use crate::io::{Document, Extracted, read_jsonl_stdin, write_jsonl_stdout};
use anyhow::Context;

const MAX_TOKENS: u32 = 512;

/// Cap per-PDF size. Anthropic's document block limit is ~32 MB; we stop well
/// before that so a runaway attachment doesn't blow up a whole batch.
const MAX_PDF_BYTES: u64 = 16 * 1024 * 1024;

const SYSTEM_PROMPT: &str = r#"You extract structured fields from a single Japanese invoice or receipt PDF.

Return ONE JSON object and nothing else (no prose, no markdown fence). Schema:

  {
    "transaction_date": "YYYY-MM-DD",      // 取引年月日 — the issue/transaction date on the invoice
    "total_amount_jpy": <integer>,         // 合計金額 in JPY, including tax. Integer yen, no commas
    "counterparty_name": "<string>",       // 取引先 — the issuing company / shop name as printed
    "counterparty_t_number": "T1234567890123" | null,   // 適格請求書登録番号 if printed: literal "T" + 13 digits
    "confidence": <number 0..1>            // your self-assessed confidence the above fields are right
  }

Rules:
- transaction_date MUST be ISO 8601 (YYYY-MM-DD). Convert Japanese dates (令和n年, n月d日) to Gregorian.
- total_amount_jpy is the customer-visible total (税込合計). No decimals — JPY has no minor unit.
- counterparty_t_number must match exactly /^T\d{13}$/ or be null. Do not invent one.
- If a field is genuinely unreadable, lower `confidence` rather than guessing wildly.
"#;

pub async fn run(args: ClassifyArgs) -> anyhow::Result<()> {
    let mut client = Client::from_env()?;
    if let Some(model) = args.model {
        client.set_model(model);
    }

    for line in read_jsonl_stdin::<Document>() {
        let mut doc = line.context("reading Document JSONL from stdin")?;
        match classify_one(&client, &doc).await {
            Ok(extracted) => {
                doc.extracted = Some(extracted);
                // Leave status as-is (default "ok"). If an upstream stage
                // already marked it needs_review, we don't override that.
            }
            Err(e) => {
                eprintln!(
                    "classify: {} ({}): {}",
                    doc.external_id,
                    doc.attachment_path.as_deref().unwrap_or("<no path>"),
                    e
                );
                doc.status = "needs_review".to_string();
            }
        }
        write_jsonl_stdout(&doc)?;
    }
    Ok(())
}

async fn classify_one(client: &Client, doc: &Document) -> anyhow::Result<Extracted> {
    let path = doc
        .attachment_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("document has no attachment_path"))?;

    let metadata = std::fs::metadata(path).with_context(|| format!("stat attachment {path}"))?;
    if metadata.len() > MAX_PDF_BYTES {
        anyhow::bail!(
            "attachment {} is {} bytes (limit {})",
            path,
            metadata.len(),
            MAX_PDF_BYTES
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading attachment {path}"))?;

    let blocks = vec![
        UserBlock::Pdf(bytes),
        UserBlock::Text(
            "Extract the fields per the schema in the system prompt. Reply with JSON only."
                .to_string(),
        ),
    ];

    let raw = client
        .complete_with_blocks(SYSTEM_PROMPT, &blocks, MAX_TOKENS)
        .await
        .context("calling Anthropic Messages API")?;

    parse_extracted(&raw).with_context(|| format!("parsing model response: {raw}"))
}

/// Parse the model's JSON reply into [`Extracted`], stripping a fenced code
/// block if the model added one despite instructions.
fn parse_extracted(raw: &str) -> anyhow::Result<Extracted> {
    let json = strip_code_fence(raw);
    let parsed: Extracted = serde_json::from_str(json)?;
    validate(&parsed)?;
    Ok(parsed)
}

fn validate(ex: &Extracted) -> anyhow::Result<()> {
    if chrono::NaiveDate::parse_from_str(&ex.transaction_date, "%Y-%m-%d").is_err() {
        anyhow::bail!(
            "transaction_date {:?} is not YYYY-MM-DD",
            ex.transaction_date
        );
    }
    if let Some(t) = &ex.counterparty_t_number {
        if !crate::invoicing_jp::tnumber::is_valid_format(t) {
            anyhow::bail!("counterparty_t_number {t:?} is not 'T' + 13 digits");
        }
    }
    Ok(())
}

/// Strip a fenced code block if the model wrapped its JSON in one.
/// Tolerates ` ```json `, ` ``` `, leading/trailing whitespace.
fn strip_code_fence(s: &str) -> &str {
    let trimmed = s.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_lang = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    let body = after_lang.trim_end();
    body.strip_suffix("```").map(str::trim_end).unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_json() {
        let raw = r#"{"transaction_date":"2026-04-30","total_amount_jpy":12100,"counterparty_name":"Acme","confidence":0.94}"#;
        let ex = parse_extracted(raw).unwrap();
        assert_eq!(ex.transaction_date, "2026-04-30");
        assert_eq!(ex.total_amount_jpy, 12100);
        assert_eq!(ex.counterparty_name, "Acme");
        assert!(ex.counterparty_t_number.is_none());
        assert!((ex.confidence - 0.94).abs() < 1e-9);
    }

    #[test]
    fn parse_with_t_number() {
        let raw = r#"{"transaction_date":"2026-04-30","total_amount_jpy":1100,"counterparty_name":"Acme","counterparty_t_number":"T1234567890123","confidence":0.9}"#;
        let ex = parse_extracted(raw).unwrap();
        assert_eq!(ex.counterparty_t_number.as_deref(), Some("T1234567890123"));
    }

    #[test]
    fn parse_strips_code_fence() {
        let raw = "```json\n{\"transaction_date\":\"2026-04-30\",\"total_amount_jpy\":1,\"counterparty_name\":\"X\",\"confidence\":0.5}\n```";
        let ex = parse_extracted(raw).unwrap();
        assert_eq!(ex.counterparty_name, "X");
    }

    #[test]
    fn rejects_bad_date() {
        let raw = r#"{"transaction_date":"2026/04/30","total_amount_jpy":1,"counterparty_name":"X","confidence":0.5}"#;
        let err = parse_extracted(raw).unwrap_err();
        assert!(err.to_string().contains("transaction_date"));
    }

    #[test]
    fn rejects_bad_t_number() {
        let raw = r#"{"transaction_date":"2026-04-30","total_amount_jpy":1,"counterparty_name":"X","counterparty_t_number":"T123","confidence":0.5}"#;
        let err = parse_extracted(raw).unwrap_err();
        assert!(err.to_string().contains("counterparty_t_number"));
    }

    #[test]
    fn rejects_bad_json() {
        let err = parse_extracted("not json").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
