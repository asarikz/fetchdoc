//! `fetch gmail` — pull invoice attachments from Gmail.
//!
//! Each matched message is fetched with `format=raw`, the base64url-encoded
//! RFC 822 bytes are decoded and written to `<cache_dir>/<messageId>.eml`,
//! and the parsed message is handed to [`mail::process_parsed_message`]. That
//! keeps Gmail aligned with the local-mail sources: PDF attachments turn into
//! one Document per PDF, and PDF-less receipts (Stripe / SaaS-style HTML mail)
//! flow through the body-primary path so `render-body` can later turn them
//! into PDFs for 電帳法 archival. The `.eml` cache also lets re-classification
//! work without re-hitting Gmail.

use crate::auth::google::{ClientSecret, refresh_access_token};
use crate::auth::paths::client_secret_path;
use crate::auth::storage;
use crate::io::write_jsonl_stdout;
use crate::sources::mail;
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Datelike, NaiveDate};
use clap::Args;
use mailparse::parse_mail;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::path::PathBuf;

const PROGRESS_TAG: &str = "fetch gmail";
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const KEYCHAIN_ACCOUNT: &str = "gmail:default";

#[derive(Args, Debug)]
pub struct GmailArgs {
    /// Only fetch messages received on or after this date (YYYY-MM-DD).
    /// Translated to Gmail's `after:YYYY/MM/DD` server-side filter, then
    /// re-checked against each message's `Date:` header.
    #[arg(long)]
    pub since: Option<String>,

    /// Gmail search query (e.g. `"has:attachment filename:pdf 請求書"`).
    #[arg(long, default_value = "has:attachment filename:pdf")]
    pub query: String,

    /// Stop after emitting this many Document records.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Directory to cache downloaded `.eml` files and extracted PDF
    /// attachments. Defaults to `<os-cache>/fetchdoc/gmail-attachments/`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Suppress per-message stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: GmailArgs) -> Result<()> {
    let cache_dir = match args.cache_dir.clone() {
        Some(p) => p,
        None => mail::default_cache_dir("gmail-attachments")?,
    };
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let since_date = match args.since.as_deref() {
        Some(s) => Some(
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .with_context(|| format!("parsing --since {s}"))?,
        ),
        None => None,
    };
    let query = build_query(&args.query, since_date);

    let secret_path = client_secret_path()?;
    let secret = ClientSecret::load(&secret_path).with_context(|| {
        format!(
            "loading {}. Run `fetchdoc auth init` first.",
            secret_path.display()
        )
    })?;
    let refresh = storage::load(KEYCHAIN_ACCOUNT)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no Gmail refresh token in OS keychain — run `fetchdoc auth login --source gmail`"
        )
    })?;

    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;
    let access_token = refresh_access_token(&secret, &refresh, &http)
        .await
        .context("refreshing Gmail access token")?;

    let mut emitted = 0usize;
    let mut page_token: Option<String> = None;
    'outer: loop {
        let list = list_messages(&http, &access_token, &query, page_token.as_deref()).await?;
        if !args.quiet {
            eprintln!(
                "{PROGRESS_TAG}: page returned {} message(s)",
                list.messages.len()
            );
        }

        for stub in &list.messages {
            let raw_message = match get_raw_message(&http, &access_token, &stub.id).await {
                Ok(m) => m,
                Err(e) => {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: skipping {}: {e:#}", stub.id);
                    }
                    continue;
                }
            };

            let raw_bytes = match decode_raw(&raw_message.raw) {
                Ok(b) => b,
                Err(e) => {
                    if !args.quiet {
                        eprintln!(
                            "{PROGRESS_TAG}: skipping {}: cannot decode raw: {e:#}",
                            stub.id
                        );
                    }
                    continue;
                }
            };

            let eml_path = cache_dir.join(format!("{}.eml", mail::sanitize_filename(&stub.id)));
            std::fs::write(&eml_path, &raw_bytes)
                .with_context(|| format!("writing {}", eml_path.display()))?;

            let parsed = match parse_mail(&raw_bytes) {
                Ok(p) => p,
                Err(e) => {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: skipping {}: parse error: {e:#}", stub.id);
                    }
                    continue;
                }
            };

            let mut extra_meta = Map::new();
            extra_meta.insert(
                "gmail_message_id".to_string(),
                Value::String(stub.id.clone()),
            );
            extra_meta.insert(
                "gmail_thread_id".to_string(),
                Value::String(raw_message.thread_id.clone()),
            );
            if !raw_message.label_ids.is_empty() {
                extra_meta.insert(
                    "gmail_label_ids".to_string(),
                    Value::Array(
                        raw_message
                            .label_ids
                            .iter()
                            .map(|s| Value::String(s.clone()))
                            .collect(),
                    ),
                );
            }
            extra_meta.insert(
                "eml_path".to_string(),
                Value::String(eml_path.to_string_lossy().into_owned()),
            );

            let label = format!("gmail:{}", stub.id);
            let id_seed = stub.id.clone();
            let opts = mail::ProcessOpts {
                source: "gmail",
                cache_dir: &cache_dir,
                since: since_date,
                fallback_seeds: &[id_seed.as_bytes()],
                extra_meta,
                progress_tag: PROGRESS_TAG,
                progress_label: &label,
                quiet: args.quiet,
                raw_bytes: &raw_bytes,
                eml_on_disk: Some(&eml_path),
            };
            let records = match mail::process_parsed_message(&parsed, &opts) {
                Ok(r) => r,
                Err(e) => {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: skipping {label}: {e:#}");
                    }
                    continue;
                }
            };
            for rec in records {
                write_jsonl_stdout(&rec)?;
                emitted += 1;
                if let Some(limit) = args.limit
                    && emitted >= limit
                {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: reached --limit {limit}");
                    }
                    break 'outer;
                }
            }
        }

        match list.next_page_token {
            Some(t) => page_token = Some(t),
            None => break,
        }
    }

    if !args.quiet {
        eprintln!("{PROGRESS_TAG}: emitted {emitted} document(s)");
    }
    Ok(())
}

/// Append `after:YYYY/MM/DD` (Gmail's date filter syntax) to the user-supplied
/// query when `--since` is set. The `mail::ProcessOpts::since` filter still
/// runs as a defence in depth: Gmail's `after:` is interpreted at day
/// granularity in the user's timezone, which can let through messages whose
/// `Date:` header is a few hours older than the requested cutoff.
fn build_query(user_query: &str, since: Option<NaiveDate>) -> String {
    match since {
        Some(d) => format!(
            "{} after:{}/{:02}/{:02}",
            user_query,
            d.year(),
            d.month(),
            d.day()
        ),
        None => user_query.to_string(),
    }
}

/// Decode Gmail's `raw` field. The API documents url-safe base64 without
/// padding, but real responses occasionally include `=` padding — strip it
/// before handing off to the no-pad decoder.
fn decode_raw(raw: &str) -> Result<Vec<u8>> {
    let trimmed = raw.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed.as_bytes())
        .context("base64url-decoding `raw`")
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    messages: Vec<MessageStub>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageStub {
    id: String,
}

async fn list_messages(
    http: &reqwest::Client,
    token: &str,
    query: &str,
    page_token: Option<&str>,
) -> Result<ListResponse> {
    let mut url = url::Url::parse(&format!("{GMAIL_API_BASE}/messages"))
        .context("building messages.list URL")?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("q", query);
        qp.append_pair("maxResults", "100");
        if let Some(t) = page_token {
            qp.append_pair("pageToken", t);
        }
    }
    let resp = http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("calling messages.list")?;
    let status = resp.status();
    let body = resp.text().await.context("reading messages.list body")?;
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "Gmail messages.list unauthorised ({status}). The refresh token may have been \
                 revoked — run `fetchdoc auth login --source gmail` to re-authenticate. Body: {body}"
            );
        }
        anyhow::bail!("Gmail messages.list failed ({status}): {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing messages.list response: {body}"))
}

#[derive(Debug, Deserialize)]
struct RawMessageResponse {
    #[serde(rename = "threadId")]
    thread_id: String,
    #[serde(default, rename = "labelIds")]
    label_ids: Vec<String>,
    raw: String,
}

async fn get_raw_message(
    http: &reqwest::Client,
    token: &str,
    id: &str,
) -> Result<RawMessageResponse> {
    let mut url = url::Url::parse(&format!("{GMAIL_API_BASE}/messages/{id}"))
        .context("building messages.get URL")?;
    url.query_pairs_mut().append_pair("format", "raw");
    let resp = http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("calling messages.get")?;
    let status = resp.status();
    let body = resp.text().await.context("reading messages.get body")?;
    if !status.is_success() {
        anyhow::bail!("Gmail messages.get failed ({status}): {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing messages.get response: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_query_appends_after_when_since_is_set() {
        let d = NaiveDate::from_ymd_opt(2026, 4, 7).unwrap();
        let q = build_query("has:attachment filename:pdf", Some(d));
        assert_eq!(q, "has:attachment filename:pdf after:2026/04/07");
    }

    #[test]
    fn build_query_passes_through_when_since_absent() {
        let q = build_query("subject:invoice", None);
        assert_eq!(q, "subject:invoice");
    }

    #[test]
    fn decode_raw_handles_padded_and_unpadded_input() {
        let body = b"%PDF-1.4\nhello\n";
        let unpadded = URL_SAFE_NO_PAD.encode(body);
        assert_eq!(decode_raw(&unpadded).unwrap(), body);

        // Same payload, but with explicit padding (Gmail occasionally returns it).
        let padded = base64::engine::general_purpose::URL_SAFE.encode(body);
        assert_eq!(decode_raw(&padded).unwrap(), body);
    }

    #[test]
    fn decode_raw_rejects_garbage() {
        assert!(decode_raw("!!!not-base64!!!").is_err());
    }
}
