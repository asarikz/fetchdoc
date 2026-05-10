//! `fetch dir` — pull pre-downloaded PDFs (or other docs) out of a local
//! folder.
//!
//! The bridge for receipts that don't arrive by email: Amazon "領収書",
//! ヨドバシ "領収書", NURO モバイル の請求書 PDF, etc. The user downloads
//! them by hand into a watched folder and `fetchdoc fetch dir` ingests them
//! into the same `Document` JSONL the mail sources emit, so the rest of the
//! pipeline (`classify` → `export local` → `export gnucash`) is unchanged.
//!
//! `external_id` is the SHA-256 of the file contents, so re-running on the
//! same folder produces the same record (downstream `import dedup`-style
//! filters can drop duplicates).
//!
//! With `--move-to` the file is relocated after ingestion to keep the
//! source folder clean and to make re-runs trivially idempotent — if the
//! destination already holds a file with the same hash-derived name, the
//! source is left in place and *not* re-emitted, so a partially-processed
//! batch can be safely retried.

use crate::io::{Document, write_jsonl_stdout};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use clap::Args;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const PROGRESS_TAG: &str = "fetch dir";

#[derive(Args, Debug)]
pub struct DirArgs {
    /// Directory to scan recursively for matching files.
    #[arg(long)]
    pub dir: PathBuf,

    /// File extension to match, case-insensitive, without the leading dot.
    /// Repeat to match multiple extensions (e.g. `--include-ext pdf
    /// --include-ext png`). Defaults to `pdf`.
    #[arg(long = "include-ext", value_name = "EXT")]
    pub include_ext: Vec<String>,

    /// Only emit files whose mtime is on or after this date (YYYY-MM-DD,
    /// local time). Files with mtime errors are kept (better to over-emit
    /// than silently drop).
    #[arg(long)]
    pub since: Option<String>,

    /// Stop after emitting this many Document records.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Move processed files into this directory, renamed to
    /// `<sha256>.<ext>`. If a file with that name already exists in the
    /// destination (i.e. the same content was previously ingested), the
    /// source file is left alone and the record is *not* re-emitted —
    /// `import dedup`-style idempotency for the inbox workflow.
    /// When unset, files stay where they are and `attachment_path` points
    /// at the original location.
    #[arg(long)]
    pub move_to: Option<PathBuf>,

    /// Suppress per-file stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: DirArgs) -> Result<()> {
    if !args.dir.exists() {
        anyhow::bail!("--dir does not exist: {}", args.dir.display());
    }
    if !args.dir.is_dir() {
        anyhow::bail!("--dir is not a directory: {}", args.dir.display());
    }

    let exts = normalise_extensions(&args.include_ext);
    let since_cutoff = parse_since(args.since.as_deref())?;

    if let Some(dest_root) = args.move_to.as_deref() {
        if dest_root.exists() && !dest_root.is_dir() {
            anyhow::bail!("--move-to is not a directory: {}", dest_root.display());
        }
        std::fs::create_dir_all(dest_root)
            .with_context(|| format!("creating --move-to {}", dest_root.display()))?;
    }

    let mut emitted = 0usize;
    let mut skipped_dup = 0usize;
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
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !ext_matches(path, &exts) {
            continue;
        }

        match process_file(path, since_cutoff, args.move_to.as_deref(), args.quiet) {
            Ok(Some(rec)) => {
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
            Ok(None) => {
                skipped_dup += 1;
            }
            Err(e) => {
                if !args.quiet {
                    eprintln!("{PROGRESS_TAG}: skipping {}: {e:#}", path.display());
                }
            }
        }
    }

    if !args.quiet {
        eprintln!(
            "{PROGRESS_TAG}: emitted {emitted} document(s), {skipped_dup} duplicate(s) skipped"
        );
    }
    Ok(())
}

/// Normalise `--include-ext` values: strip leading `.`, lowercase, drop blanks,
/// and fall back to `["pdf"]` when the user didn't pass any.
fn normalise_extensions(input: &[String]) -> Vec<String> {
    let mut out: Vec<String> = input
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if out.is_empty() {
        out.push("pdf".to_string());
    }
    out
}

fn ext_matches(path: &Path, exts: &[String]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    exts.iter().any(|e| e == &lower)
}

/// Parse `--since YYYY-MM-DD` into a UTC cutoff at local-midnight on that day.
fn parse_since(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = s else {
        return Ok(None);
    };
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .with_context(|| format!("parsing --since {raw}"))?;
    let local_midnight = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("00:00:00 is valid"))
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous local midnight for --since {raw}"))?;
    Ok(Some(local_midnight.with_timezone(&Utc)))
}

/// Process a single matched file. Returns:
/// - `Ok(Some(doc))` — emitted (file optionally moved)
/// - `Ok(None)` — duplicate of a previously-moved file in `--move-to`, skipped
/// - `Err(_)` — read/move error; caller logs and continues
fn process_file(
    path: &Path,
    since: Option<DateTime<Utc>>,
    move_to: Option<&Path>,
    quiet: bool,
) -> Result<Option<Document>> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;

    let mtime_utc: Option<DateTime<Utc>> = metadata.modified().ok().map(DateTime::<Utc>::from);
    if let (Some(cutoff), Some(mt)) = (since, mtime_utc)
        && mt < cutoff
    {
        return Ok(None);
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let hash_hex = sha256_hex(&bytes);

    // Final on-disk path: either the original (no move), or
    // `<move-to>/<hash>.<ext>`. Same-content collisions in the destination
    // are treated as "already ingested" and skipped.
    let final_path = if let Some(dest_root) = move_to {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_else(|| "bin".to_string());
        let dest = dest_root.join(format!("{hash_hex}.{ext}"));
        if dest.exists() {
            if !quiet {
                eprintln!(
                    "{PROGRESS_TAG}: duplicate {} (already in {})",
                    path.display(),
                    dest.display()
                );
            }
            return Ok(None);
        }
        std::fs::rename(path, &dest).or_else(|_| {
            // Cross-device renames fail on Linux; fall back to copy + remove.
            std::fs::copy(path, &dest)
                .with_context(|| format!("copying {} → {}", path.display(), dest.display()))?;
            std::fs::remove_file(path)
                .with_context(|| format!("removing source {}", path.display()))?;
            Ok::<_, anyhow::Error>(())
        })?;
        dest
    } else {
        path.to_path_buf()
    };

    if !quiet {
        eprintln!(
            "{PROGRESS_TAG}: {} ({} bytes) → sha256:{}",
            path.display(),
            bytes.len(),
            &hash_hex[..16]
        );
    }

    let mtime_str = mtime_utc.map(|t| t.to_rfc3339());
    let original_path = path.to_string_lossy().into_owned();
    let meta = json!({
        "original_path": original_path,
        "mtime": mtime_str,
        "file_size": bytes.len(),
    });

    Ok(Some(Document {
        source: "dir".to_string(),
        external_id: format!("sha256:{hash_hex}"),
        attachment_path: Some(final_path.to_string_lossy().into_owned()),
        source_meta: Some(meta),
        extracted: None,
        exported: None,
        status: "ok".to_string(),
    }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_extensions_defaults_to_pdf() {
        assert_eq!(normalise_extensions(&[]), vec!["pdf".to_string()]);
    }

    #[test]
    fn normalise_extensions_strips_leading_dot_and_lowercases() {
        let got = normalise_extensions(&[".PDF".into(), "PNG".into(), "  ".into()]);
        assert_eq!(got, vec!["pdf".to_string(), "png".to_string()]);
    }

    #[test]
    fn ext_matches_is_case_insensitive() {
        let exts = vec!["pdf".to_string()];
        assert!(ext_matches(Path::new("foo.PDF"), &exts));
        assert!(ext_matches(Path::new("foo.pdf"), &exts));
        assert!(!ext_matches(Path::new("foo.txt"), &exts));
        assert!(!ext_matches(Path::new("noext"), &exts));
    }

    #[test]
    fn sha256_hex_is_64_chars_lowercase() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        // Known SHA-256 of "hello".
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
