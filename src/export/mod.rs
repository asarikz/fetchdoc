//! `export <target>` subcommand: write classified documents to a destination.
//!
//! Each export reads JSONL on stdin and re-emits the same records on stdout
//! with an `exported` field added, so multiple export targets can chain:
//!
//!   fetchdoc fetch gmail | fetchdoc classify \
//!     | fetchdoc export local --root ~/受領請求書 \
//!     | fetchdoc export gnucash --book ~/finance.gnucash

use clap::{Args, Subcommand};

mod accounts;
mod gnucash;
mod local;

#[derive(Args, Debug)]
pub struct ExportArgs {
    #[command(subcommand)]
    command: ExportCommand,
}

#[derive(Subcommand, Debug)]
enum ExportCommand {
    /// Export to a GnuCash CSV the GnuCash transaction CSV importer can read.
    Gnucash(gnucash::GnucashArgs),
    /// Write the original PDF to the local filesystem with a structured filename.
    Local(local::LocalArgs),
}

pub async fn run(args: ExportArgs) -> anyhow::Result<()> {
    match args.command {
        ExportCommand::Gnucash(a) => gnucash::run(a).await,
        ExportCommand::Local(a) => local::run(a).await,
    }
}
