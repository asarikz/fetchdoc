use clap::{Parser, Subcommand};

/// Fetch invoices from Gmail, classify with AI, export to GnuCash and more.
///
/// Each subcommand reads JSON Lines on stdin and writes JSON Lines on stdout.
/// Pipe them together to build pipelines:
///
///   fetchdoc fetch gmail --since 2026-04-01 \
///     | fetchdoc classify \
///     | fetchdoc export gnucash --book ~/finance.gnucash
#[derive(Parser, Debug)]
#[command(name = "fetchdoc", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage authentication for upstream services (Gmail, etc).
    Auth(crate::auth::AuthArgs),
    /// Fetch documents from a source. Emits JSONL on stdout.
    Fetch(crate::sources::FetchArgs),
    /// Import bank/card statements from a local file. Emits Transaction JSONL.
    Import(crate::import::ImportArgs),
    /// Classify and extract structured data from documents on stdin.
    Classify(crate::classify::ClassifyArgs),
    /// Render body-primary email records to PDF (Document JSONL → JSONL).
    /// Insert between `fetch` and `classify` when receipts may arrive in the
    /// email body itself rather than as a PDF attachment.
    RenderBody(crate::render_body::RenderBodyArgs),
    /// Export classified documents (read JSONL on stdin).
    Export(crate::export::ExportArgs),
    /// Validate a Japanese qualified-invoice T number against the NTA registry.
    VerifyTnumber {
        /// 14-character T number (`T` + 13 digits).
        tnumber: String,
    },
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Command::Auth(args) => crate::auth::run(args).await,
            Command::Fetch(args) => crate::sources::run(args).await,
            Command::Import(args) => crate::import::run(args).await,
            Command::Classify(args) => crate::classify::run(args).await,
            Command::RenderBody(args) => crate::render_body::run(args).await,
            Command::Export(args) => crate::export::run(args).await,
            Command::VerifyTnumber { tnumber } => {
                crate::invoicing_jp::verify_tnumber(&tnumber).await
            }
        }
    }
}
