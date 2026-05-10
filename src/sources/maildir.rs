//! `fetch maildir` — pull PDF attachments out of locally-stored Maildir trees.
//!
//! A Maildir is a directory containing `cur/`, `new/`, and `tmp/` subfolders.
//! Each file inside `cur/` and `new/` is one raw RFC 822 message (filenames
//! are typically `<unique>:2,<flags>` — we ignore the suffix and just parse
//! the bytes). `tmp/` is for half-written messages and is skipped.
//!
//! `--dir PATH` accepts either a single Maildir or a Maildir++ tree (a root
//! whose subdirectories — including dot-prefixed folders like `.Sent/` —
//! each contain their own `cur/new/tmp` triple). We auto-detect every
//! Maildir under the path so `~/Maildir`, `~/.mbsync`, or `~/Mail` Just Work.

use crate::io::{Document, write_jsonl_stdout};
use crate::sources::mail;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Args;
use mailparse::parse_mail;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const PROGRESS_TAG: &str = "fetch maildir";

#[derive(Args, Debug)]
pub struct MaildirArgs {
    /// Root to scan. May be a single Maildir (containing `cur/new/tmp`) or
    /// a Maildir++ tree containing many of them.
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
    /// `<os-cache>/fetchdoc/maildir-attachments/`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Suppress per-file stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: MaildirArgs) -> Result<()> {
    if !args.dir.exists() {
        anyhow::bail!("--dir does not exist: {}", args.dir.display());
    }
    if !args.dir.is_dir() {
        anyhow::bail!("--dir is not a directory: {}", args.dir.display());
    }

    let cache_dir = match args.cache_dir {
        Some(p) => p,
        None => mail::default_cache_dir("maildir-attachments")?,
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

    let maildirs = find_maildirs(&args.dir);
    if maildirs.is_empty() {
        anyhow::bail!(
            "no Maildir found under {} (a Maildir is a directory containing cur/, new/, and tmp/)",
            args.dir.display()
        );
    }

    let mut emitted = 0usize;
    'outer: for md in maildirs {
        for sub in ["new", "cur"] {
            let folder = md.join(sub);
            if !folder.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = match std::fs::read_dir(&folder) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| e.path())
                    .collect(),
                Err(e) => {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: cannot read {}: {e}", folder.display());
                    }
                    continue;
                }
            };
            // Sort for deterministic output and reproducible tests.
            files.sort();

            for path in files {
                match process_file(&md, &path, &cache_dir, since_date, args.quiet) {
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
        }
    }

    if !args.quiet {
        eprintln!("{PROGRESS_TAG}: emitted {emitted} document(s)");
    }
    Ok(())
}

/// Find every Maildir reachable from `root`, in deterministic order. A
/// directory qualifies when both `cur/` and `new/` are direct children.
/// `root` itself is included if it qualifies. We don't descend into a
/// detected Maildir's `cur/new/tmp` (those hold messages, not nested
/// folders).
fn find_maildirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            // Skip the cur/new/tmp leaves — they hold message files, not
            // nested mailbox folders. Without this we'd waste time walking
            // into them.
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "cur" | "new" | "tmp")
        });
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let p = entry.path();
        if is_maildir(p) {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn is_maildir(path: &Path) -> bool {
    path.join("cur").is_dir() && path.join("new").is_dir()
}

fn process_file(
    maildir_root: &Path,
    path: &Path,
    cache_dir: &Path,
    since: Option<NaiveDate>,
    quiet: bool,
) -> Result<Vec<Document>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_mail(&raw).context("parsing MIME message")?;

    let path_str = path.to_string_lossy();
    let root_str = maildir_root.to_string_lossy();

    let mut extra_meta = Map::new();
    extra_meta.insert(
        "maildir_path".to_string(),
        Value::String(root_str.to_string()),
    );
    extra_meta.insert(
        "message_path".to_string(),
        Value::String(path_str.to_string()),
    );

    let opts = mail::ProcessOpts {
        source: "maildir",
        cache_dir,
        since,
        fallback_seeds: &[path_str.as_bytes()],
        extra_meta,
        progress_tag: PROGRESS_TAG,
        progress_label: &path_str,
        quiet,
    };
    mail::process_parsed_message(&parsed, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn is_maildir_requires_cur_and_new() {
        let tmp = std::env::temp_dir().join(format!(
            "fetchdoc-md-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp.join("cur")).unwrap();
        std::fs::create_dir_all(tmp.join("new")).unwrap();
        std::fs::create_dir_all(tmp.join("tmp")).unwrap();
        assert!(is_maildir(&tmp));

        let just_cur = tmp.join("just_cur");
        std::fs::create_dir_all(just_cur.join("cur")).unwrap();
        assert!(!is_maildir(&just_cur));
    }

    #[test]
    fn find_maildirs_handles_maildirpp_layout() {
        let root = std::env::temp_dir().join(format!(
            "fetchdoc-md-find-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // INBOX
        touch(&root.join("cur").join(".keep"));
        touch(&root.join("new").join(".keep"));
        touch(&root.join("tmp").join(".keep"));
        // Maildir++ subfolder .Sent
        touch(&root.join(".Sent").join("cur").join(".keep"));
        touch(&root.join(".Sent").join("new").join(".keep"));
        touch(&root.join(".Sent").join("tmp").join(".keep"));
        // Plain non-maildir directory — should be ignored.
        std::fs::create_dir_all(root.join("notes")).unwrap();

        let mut found = find_maildirs(&root);
        found.sort();
        assert_eq!(found.len(), 2, "found: {found:?}");
        assert!(found.iter().any(|p| p == &root));
        assert!(found.iter().any(|p| p == &root.join(".Sent")));
    }
}
