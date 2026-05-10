//! Anthropic Claude OCR/extraction backend.
//!
//! Reads `ANTHROPIC_API_KEY` from the environment, sends each PDF as a
//! `document` content block, and asks Claude to extract structured fields.

use crate::classify::ClassifyArgs;

#[allow(dead_code)]
const DEFAULT_MODEL: &str = "claude-sonnet-4-7";

#[allow(dead_code)]
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

pub async fn run(_args: ClassifyArgs) -> anyhow::Result<()> {
    anyhow::bail!("classify --ocr=anthropic: not implemented yet (see issue #18)")
}
