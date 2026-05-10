//! `import <kind>` subcommand: ingest bank/card statements from local files.
//!
//! Each importer reads a CSV / xlsx file and emits one [`Transaction`](crate::io::Transaction)
//! JSONL record per row. Format quirks are pushed into a per-source TOML
//! "profile" so the importer itself stays format-agnostic. Profiles can be
//! hand-written or generated once by `--infer` (next iteration).
//!
//! ```text
//! fetchdoc import csv --profile smbc input.csv > out.jsonl
//! ```

use clap::{Args, Subcommand};

mod csv;
mod infer;
mod profile;

pub use profile::Profile;

#[derive(Args, Debug)]
pub struct ImportArgs {
    #[command(subcommand)]
    command: ImportCommand,
}

#[derive(Subcommand, Debug)]
enum ImportCommand {
    /// Import a delimited text file (CSV / TSV) into Transaction JSONL.
    Csv(csv::CsvArgs),
}

pub async fn run(args: ImportArgs) -> anyhow::Result<()> {
    match args.command {
        ImportCommand::Csv(a) => csv::run(a).await,
    }
}
