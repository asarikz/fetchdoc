//! `fetch <source>` subcommand: pull documents from upstream services.
//!
//! Each source emits one JSON Lines record per document on stdout. The
//! attachment itself is written to a local cache directory and referenced
//! by `attachment_path` in the record.

use clap::{Args, Subcommand};

mod gmail;

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[command(subcommand)]
    command: FetchCommand,
}

#[derive(Subcommand, Debug)]
enum FetchCommand {
    /// Fetch invoice attachments from Gmail.
    Gmail(gmail::GmailArgs),
}

pub async fn run(args: FetchArgs) -> anyhow::Result<()> {
    match args.command {
        FetchCommand::Gmail(a) => gmail::run(a).await,
    }
}
