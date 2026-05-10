//! `classify` subcommand: read documents from stdin, run OCR/extraction,
//! emit enriched documents on stdout.
//!
//! Multiple OCR backends are pluggable. The default `anthropic` backend
//! uses Claude (`ANTHROPIC_API_KEY` env var) and works without any GCP
//! setup. The `vertex` backend uses Gemini in your own GCP project.

use clap::{Args, ValueEnum};

mod anthropic;

#[derive(Args, Debug)]
pub struct ClassifyArgs {
    /// OCR backend to use.
    #[arg(long, value_enum, default_value_t = OcrBackend::Anthropic)]
    pub ocr: OcrBackend,

    /// Override the model name (backend-specific).
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum OcrBackend {
    Anthropic,
    Vertex,
    Openai,
}

pub async fn run(args: ClassifyArgs) -> anyhow::Result<()> {
    match args.ocr {
        OcrBackend::Anthropic => anthropic::run(args).await,
        OcrBackend::Vertex => anyhow::bail!("vertex backend: not implemented yet"),
        OcrBackend::Openai => anyhow::bail!("openai backend: not implemented yet"),
    }
}
