//! `fetch eml` — pull PDF attachments out of locally-stored `.eml` files.
//!
//! Walks `--dir` recursively, parses each `.eml` with `mailparse`, writes any
//! PDF attachments into a cache directory, and emits one Document JSONL record
//! per attachment on stdout. Unreadable files are skipped with a stderr
//! warning so a single broken message never aborts the whole run.

use crate::io::{Document, write_jsonl_stdout};
use crate::sources::mail;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Args;
use mailparse::parse_mail;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const PROGRESS_TAG: &str = "fetch eml";

#[derive(Args, Debug)]
pub struct EmlArgs {
    /// Directory to scan recursively for `*.eml` files (case-insensitive).
    #[arg(long)]
    pub dir: PathBuf,

    /// Only emit messages whose `Date:` header is on or after this date
    /// (YYYY-MM-DD). Messages with an unparseable Date header are kept.
    #[arg(long)]
    pub since: Option<String>,

    /// Stop after emitting this many Document records.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Directory to write extracted PDF attachments. Defaults to
    /// `<os-cache>/fetchdoc/eml-attachments/`. Resolved using only
    /// `std::env` (no extra dependency on `dirs`).
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Suppress per-file stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: EmlArgs) -> Result<()> {
    if !args.dir.exists() {
        anyhow::bail!("--dir does not exist: {}", args.dir.display());
    }
    if !args.dir.is_dir() {
        anyhow::bail!("--dir is not a directory: {}", args.dir.display());
    }

    let cache_dir = match args.cache_dir {
        Some(p) => p,
        None => mail::default_cache_dir("eml-attachments")?,
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

    let mut emitted = 0usize;
    let walker = WalkDir::new(&args.dir).sort_by_file_name().into_iter();
    'outer: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if !args.quiet {
                    eprintln!("{PROGRESS_TAG}: walk error: {e}");
                }
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_eml(entry.path()) {
            continue;
        }

        let path = entry.path();
        match process_file(path, &cache_dir, since_date, args.quiet) {
            Ok(records) => {
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
            Err(e) => {
                if !args.quiet {
                    eprintln!("{PROGRESS_TAG}: skipping {}: {e:#}", path.display());
                }
            }
        }
    }

    if !args.quiet {
        eprintln!("{PROGRESS_TAG}: emitted {emitted} document(s)");
    }
    Ok(())
}

fn is_eml(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("eml"))
        .unwrap_or(false)
}

fn process_file(
    path: &Path,
    cache_dir: &Path,
    since: Option<NaiveDate>,
    quiet: bool,
) -> Result<Vec<Document>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_mail(&raw).context("parsing MIME message")?;

    let path_str = path.to_string_lossy();
    let mut extra_meta = Map::new();
    extra_meta.insert("eml_path".to_string(), Value::String(path_str.to_string()));

    let path_bytes = path_str.as_bytes();
    let opts = mail::ProcessOpts {
        source: "eml",
        cache_dir,
        since,
        fallback_seeds: &[path_bytes],
        extra_meta,
        progress_tag: PROGRESS_TAG,
        progress_label: &path_str,
        quiet,
        raw_bytes: &raw,
        // The .eml is already on disk at the user's path — point body-primary
        // records there directly instead of duplicating into the cache.
        eml_on_disk: Some(path),
    };
    mail::process_parsed_message(&parsed, &opts)
}
