//! Minimal Anthropic Messages API client.
//!
//! Shared between `import csv --infer` (one-shot profile generation) and
//! eventually `classify --ocr=anthropic`. Intentionally tiny: a single text
//! completion call, JSON in / JSON out, no streaming, no tool use, no
//! per-call retries (the caller decides whether to retry).
//!
//! Auth: `ANTHROPIC_API_KEY` from the environment. Model: defaults to
//! [`DEFAULT_MODEL`], overridable per-call or via `FETCHDOC_ANTHROPIC_MODEL`.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default model. Sonnet 4.6 is the current Anthropic Sonnet release and a
/// good balance of cost and quality for one-shot inference tasks.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// HTTP client + auth + chosen model. Cheap to clone (wraps an `Arc` inside
/// `reqwest::Client`).
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

impl Client {
    /// Build a client from the environment.
    ///
    /// - `ANTHROPIC_API_KEY` (required) — auth header.
    /// - `FETCHDOC_ANTHROPIC_MODEL` (optional) — overrides [`DEFAULT_MODEL`].
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "ANTHROPIC_API_KEY is not set. Get a key at https://console.anthropic.com/ \
                 and export it before running this command."
            )
        })?;
        let model =
            std::env::var("FETCHDOC_ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            api_key,
            model,
        })
    }

    /// Single-turn text completion. Returns the concatenated text content of
    /// the assistant reply.
    pub async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        self.complete_with_blocks(system, &[UserBlock::Text(user.into())], max_tokens)
            .await
    }

    /// Single-turn completion with a heterogeneous user message — text + PDF
    /// document blocks. PDF bytes are base64-encoded and sent as `document`
    /// content blocks per the Messages API spec.
    pub async fn complete_with_blocks(
        &self,
        system: &str,
        blocks: &[UserBlock],
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        self.complete_full(SystemPrompt::Plain(system), blocks, max_tokens)
            .await
    }

    /// Like [`Self::complete_with_blocks`] but lets the caller mark the system
    /// prompt as cacheable. Use when a long static prefix (e.g. a chart of
    /// accounts) is reused across many calls — the API caches it for ~5 min
    /// at a fraction of the per-call cost.
    pub async fn complete_with_cached_system(
        &self,
        system: &str,
        blocks: &[UserBlock],
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        self.complete_full(SystemPrompt::Cached(system), blocks, max_tokens)
            .await
    }

    async fn complete_full(
        &self,
        system: SystemPrompt<'_>,
        blocks: &[UserBlock],
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        let content: Vec<ContentBlockOut> = blocks
            .iter()
            .map(|b| match b {
                UserBlock::Text(t) => ContentBlockOut::Text { text: t.clone() },
                UserBlock::Pdf(bytes) => ContentBlockOut::Document {
                    source: DocumentSource {
                        kind: "base64",
                        media_type: "application/pdf",
                        data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    },
                },
            })
            .collect();

        let req = MessagesRequest {
            model: &self.model,
            max_tokens,
            system: match system {
                SystemPrompt::Plain(s) => Some(SystemField::Plain(s)),
                SystemPrompt::Cached(s) => Some(SystemField::Blocks(vec![SystemBlock {
                    kind: "text",
                    text: s,
                    cache_control: Some(CacheControl { kind: "ephemeral" }),
                }])),
            },
            messages: vec![BlockMessage {
                role: "user",
                content,
            }],
        };

        let resp = self
            .http
            .post(MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&req)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("Anthropic API {}: {}", status.as_u16(), body);
        }
        extract_text(&body)
    }

    /// Model id this client will use. Useful for log lines.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Override the model id (e.g. from a `--model` CLI flag).
    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }
}

/// One piece of a user message. `Text` for prose, `Pdf` for a PDF attachment
/// the model should read as a `document` content block.
pub enum UserBlock {
    Text(String),
    Pdf(Vec<u8>),
}

enum SystemPrompt<'a> {
    Plain(&'a str),
    Cached(&'a str),
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemField<'a>>,
    messages: Vec<BlockMessage<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SystemField<'a> {
    Plain(&'a str),
    Blocks(Vec<SystemBlock<'a>>),
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct BlockMessage<'a> {
    role: &'a str,
    content: Vec<ContentBlockOut>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockOut {
    Text { text: String },
    Document { source: DocumentSource },
}

#[derive(Serialize)]
struct DocumentSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'static str,
    data: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// Pull the concatenated text content out of a Messages API response body.
/// Public-in-crate so unit tests (and the inference command) can exercise it
/// without hitting the network.
pub(crate) fn extract_text(body: &str) -> anyhow::Result<String> {
    let parsed: MessagesResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Anthropic response not JSON: {e}; body={body}"))?;
    let mut out = String::new();
    for block in parsed.content {
        if block.kind == "text" {
            if let Some(t) = block.text {
                out.push_str(&t);
            }
        }
    }
    if out.is_empty() {
        anyhow::bail!("Anthropic response had no text content; body={body}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_single_block() {
        let body = r#"{"content":[{"type":"text","text":"hello"}]}"#;
        assert_eq!(extract_text(body).unwrap(), "hello");
    }

    #[test]
    fn extract_text_concatenates_blocks() {
        let body = r#"{"content":[{"type":"text","text":"foo"},{"type":"text","text":"bar"}]}"#;
        assert_eq!(extract_text(body).unwrap(), "foobar");
    }

    #[test]
    fn extract_text_skips_non_text_blocks() {
        let body =
            r#"{"content":[{"type":"thinking","text":"ignore"},{"type":"text","text":"keep"}]}"#;
        assert_eq!(extract_text(body).unwrap(), "keep");
    }

    #[test]
    fn extract_text_errors_on_empty() {
        let body = r#"{"content":[]}"#;
        assert!(extract_text(body).is_err());
    }

    #[test]
    fn extract_text_errors_on_malformed_json() {
        assert!(extract_text("not json").is_err());
    }
}
