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

pub(crate) mod csv;
mod dedup;
mod infer;
mod profile;
mod xlsx;

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
    /// Import an .xlsx workbook into Transaction JSONL.
    Xlsx(xlsx::XlsxArgs),
    /// Drop Transaction records whose external_id already appears in
    /// a previous JSONL file. Idempotent re-imports.
    Dedup(dedup::DedupArgs),
}

pub async fn run(args: ImportArgs) -> anyhow::Result<()> {
    match args.command {
        ImportCommand::Csv(a) => csv::run(a).await,
        ImportCommand::Xlsx(a) => xlsx::run(a).await,
        ImportCommand::Dedup(a) => dedup::run(a).await,
    }
}
