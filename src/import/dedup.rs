//! `import dedup` — drop Transaction records whose `external_id` already
//! appears in a previous JSONL file. Idempotent re-imports for the
//! re-import-the-whole-statement-every-month workflow.
//!
//! ```text
//! fetchdoc import csv --profile smbc statement.csv \
//!   | fetchdoc import dedup --against ~/finance/smbc.all.jsonl \
//!   | tee -a ~/finance/smbc.all.jsonl
//! ```

use crate::io::{Transaction, read_jsonl_stdin, write_jsonl_stdout};
use anyhow::Context;
use clap::Args;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct DedupArgs {
    /// JSONL file of previously-imported Transaction records. Their
    /// `external_id` values are loaded into memory and used to filter stdin.
    /// May be specified multiple times to merge several histories.
    #[arg(long, required = true)]
    pub against: Vec<String>,

    /// Suppress per-row stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: DedupArgs) -> anyhow::Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for path in &args.against {
        load_ids_into(path, &mut seen)
            .with_context(|| format!("loading external_ids from {path}"))?;
    }
    if !args.quiet {
        eprintln!("dedup: loaded {} known external_id(s)", seen.len());
    }

    let mut kept = 0usize;
    let mut dropped = 0usize;
    for rec in read_jsonl_stdin::<Transaction>() {
        let tx = rec.context("reading Transaction JSONL on stdin")?;
        if seen.contains(&tx.external_id) {
            dropped += 1;
        } else {
            // Insert so we also dedup duplicates *within* the input stream.
            seen.insert(tx.external_id.clone());
            write_jsonl_stdout(&tx)?;
            kept += 1;
        }
    }

    if !args.quiet {
        eprintln!("dedup: kept {kept}, dropped {dropped}");
    }
    Ok(())
}

fn load_ids_into(path: &str, set: &mut HashSet<String>) -> anyhow::Result<()> {
    let f = std::fs::File::open(PathBuf::from(path))?;
    let r = std::io::BufReader::new(f);
    for (line_no, line) in r.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Only pull external_id — full Transaction parsing isn't needed and
        // tolerates schema drift (e.g. an old file with extra fields).
        #[derive(serde::Deserialize)]
        struct Stub {
            external_id: String,
        }
        match serde_json::from_str::<Stub>(&line) {
            Ok(s) => {
                set.insert(s.external_id);
            }
            Err(e) => {
                eprintln!("warning: {path}:{} skip unparseable line: {e}", line_no + 1);
            }
        }
    }
    Ok(())
}
