//! `fetch mbox` — pull PDF attachments out of locally-stored mbox archives.
//!
//! Mbox is a single file holding many concatenated messages, separated by
//! lines starting with `From `. It's the format Apple Mail (`*.mbox/mbox`),
//! Thunderbird, Google Takeout, and `mbsync --create-store mboxrd` all use.
//!
//! Either `--file PATH` (one mbox) or `--dir PATH` (recurse, picking up
//! `*.mbox` files plus Apple Mail's bare-`mbox` files inside `*.mbox/`
//! bundles) is required.
//!
//! The parser splits on any line beginning with `From ` (canonical mboxrd
//! separator, escaped as `>From ` in message bodies). Lines starting with
//! `>+From ` inside a message body are unescaped (one `>` removed) before
//! handing the bytes to `mailparse`. Bad messages are skipped with a stderr
//! warning so a single corrupt entry doesn't abort the whole archive.

use crate::io::{Document, write_jsonl_stdout};
use crate::sources::mail;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Args;
use mailparse::parse_mail;
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const PROGRESS_TAG: &str = "fetch mbox";

#[derive(Args, Debug)]
pub struct MboxArgs {
    /// Path to a single mbox file. Mutually exclusive with `--dir`.
    #[arg(long, conflicts_with = "dir", required_unless_present = "dir")]
    pub file: Option<PathBuf>,

    /// Directory to scan recursively for mbox files. Picks up files with a
    /// `.mbox` extension plus bare `mbox` files inside `*.mbox/` bundles
    /// (the layout Apple Mail's *Save Mailbox* writes).
    #[arg(long, conflicts_with = "file")]
    pub dir: Option<PathBuf>,

    /// Only emit messages whose `Date:` header is on or after this date
    /// (YYYY-MM-DD). Messages with an unparseable Date header are kept.
    #[arg(long)]
    pub since: Option<String>,

    /// Stop after emitting this many Document records.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Directory to write extracted PDF attachments. Defaults to
    /// `<os-cache>/fetchdoc/mbox-attachments/`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Suppress per-message stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: MboxArgs) -> Result<()> {
    let cache_dir = match args.cache_dir {
        Some(p) => p,
        None => mail::default_cache_dir("mbox-attachments")?,
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

    let files = collect_mbox_files(args.file.as_deref(), args.dir.as_deref(), args.quiet)?;

    let mut emitted = 0usize;
    'outer: for path in files {
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                if !args.quiet {
                    eprintln!("{PROGRESS_TAG}: cannot open {}: {e}", path.display());
                }
                continue;
            }
        };
        let reader = BufReader::new(file);

        for (idx, msg_res) in iter_messages(reader).enumerate() {
            let bytes = match msg_res {
                Ok(b) => b,
                Err(e) => {
                    if !args.quiet {
                        eprintln!("{PROGRESS_TAG}: read error in {}: {e}", path.display());
                    }
                    break;
                }
            };
            let label = format!("{}#{idx}", path.display());
            match process_message(&path, idx, &bytes, &cache_dir, since_date, args.quiet) {
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
                        eprintln!("{PROGRESS_TAG}: skipping {label}: {e:#}");
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

/// Resolve `--file` / `--dir` into a list of mbox files to scan, in
/// deterministic order. `clap` already enforces that exactly one of the two
/// is set, so this only deals with directory walking.
fn collect_mbox_files(
    file: Option<&Path>,
    dir: Option<&Path>,
    quiet: bool,
) -> Result<Vec<PathBuf>> {
    if let Some(f) = file {
        if !f.is_file() {
            anyhow::bail!("--file is not a regular file: {}", f.display());
        }
        return Ok(vec![f.to_path_buf()]);
    }
    let d = dir.expect("clap guarantees --dir when --file is absent");
    if !d.is_dir() {
        anyhow::bail!("--dir is not a directory: {}", d.display());
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(d).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if !quiet {
                    eprintln!("{PROGRESS_TAG}: walk error: {e}");
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if is_mbox_file(p) {
            out.push(p.to_path_buf());
        }
    }
    Ok(out)
}

/// Match mbox files by name. The two real-world cases:
/// 1. `*.mbox` — Thunderbird per-folder files, Takeout's `All mail.mbox`,
///    `mbsync` mboxrd output.
/// 2. A bare file literally named `mbox` (no extension) sitting inside an
///    `*.mbox/` bundle — Apple Mail's *Save Mailbox* layout.
fn is_mbox_file(path: &Path) -> bool {
    let ext_match = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("mbox"))
        .unwrap_or(false);
    if ext_match {
        return true;
    }
    let name_is_mbox = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("mbox"))
        .unwrap_or(false);
    let parent_is_mbox_bundle = path
        .parent()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("mbox"))
        .unwrap_or(false);
    name_is_mbox && parent_is_mbox_bundle
}

fn process_message(
    mbox_path: &Path,
    index: usize,
    raw: &[u8],
    cache_dir: &Path,
    since: Option<NaiveDate>,
    quiet: bool,
) -> Result<Vec<Document>> {
    let parsed = parse_mail(raw).context("parsing MIME message")?;

    let path_str = mbox_path.to_string_lossy();
    let label = format!("{path_str}#{index}");
    let idx_str = index.to_string();

    let mut extra_meta = Map::new();
    extra_meta.insert("mbox_path".to_string(), Value::String(path_str.to_string()));
    extra_meta.insert(
        "mbox_index".to_string(),
        Value::Number(serde_json::Number::from(index as u64)),
    );

    let opts = mail::ProcessOpts {
        source: "mbox",
        cache_dir,
        since,
        fallback_seeds: &[path_str.as_bytes(), idx_str.as_bytes()],
        extra_meta,
        progress_tag: PROGRESS_TAG,
        progress_label: &label,
        quiet,
    };
    mail::process_parsed_message(&parsed, &opts)
}

/// Stream-friendly iterator over the raw bytes of each message in an mbox.
///
/// The grammar we accept: a message starts with a line beginning with
/// `From ` and continues until the next such line (or EOF). This is the
/// canonical mboxrd separator and the de-facto standard for `mbsync`,
/// Apple Mail, Thunderbird, and Google Takeout. Anything before the first
/// `From ` line is silently skipped (some tools prepend a banner).
///
/// `>+From ` lines inside the body are mboxrd-unescaped (one leading `>` is
/// stripped) so that downstream MIME parsing sees the original bytes.
fn iter_messages<R: BufRead>(reader: R) -> impl Iterator<Item = std::io::Result<Vec<u8>>> {
    MboxIter {
        reader,
        line_buf: Vec::new(),
        current: Vec::new(),
        seen_first: false,
        done: false,
    }
}

struct MboxIter<R: BufRead> {
    reader: R,
    line_buf: Vec<u8>,
    current: Vec<u8>,
    seen_first: bool,
    done: bool,
}

impl<R: BufRead> Iterator for MboxIter<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            self.line_buf.clear();
            let n = match self.reader.read_until(b'\n', &mut self.line_buf) {
                Ok(n) => n,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            if n == 0 {
                self.done = true;
                if !self.current.is_empty() {
                    let msg = std::mem::take(&mut self.current);
                    return Some(Ok(strip_trailing_blank_line(msg)));
                }
                return None;
            }

            if self.line_buf.starts_with(b"From ") {
                if self.seen_first {
                    let msg = std::mem::take(&mut self.current);
                    return Some(Ok(strip_trailing_blank_line(msg)));
                }
                // First "From " line we've seen — it marks the start of the
                // first message. Anything before it (banners, garbage) is
                // discarded. Don't include the line itself in the body.
                self.seen_first = true;
                continue;
            }
            if !self.seen_first {
                continue;
            }
            // mboxrd un-escape: `>From ` → `From `, `>>From ` → `>From `, ...
            let to_append: &[u8] = if line_starts_with_escaped_from(&self.line_buf) {
                &self.line_buf[1..]
            } else {
                &self.line_buf
            };
            self.current.extend_from_slice(to_append);
        }
    }
}

fn line_starts_with_escaped_from(line: &[u8]) -> bool {
    let mut i = 0;
    while i < line.len() && line[i] == b'>' {
        i += 1;
    }
    i > 0 && line[i..].starts_with(b"From ")
}

/// Mbox messages are conventionally followed by one blank line before the
/// next `From ` separator. Drop it so the message bytes match the on-disk
/// `.eml` form (and so the body doesn't gain spurious trailing whitespace).
fn strip_trailing_blank_line(mut msg: Vec<u8>) -> Vec<u8> {
    if msg.ends_with(b"\r\n\r\n") {
        msg.truncate(msg.len() - 2);
    } else if msg.ends_with(b"\n\n") {
        msg.truncate(msg.len() - 1);
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn collect(bytes: &[u8]) -> Vec<Vec<u8>> {
        iter_messages(Cursor::new(bytes))
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn splits_on_from_lines() {
        let mbox = b"From sender@example.com Wed Apr 30 10:00:00 2026\n\
                     Subject: one\n\
                     \n\
                     body one\n\
                     \n\
                     From sender@example.com Thu May 01 10:00:00 2026\n\
                     Subject: two\n\
                     \n\
                     body two\n";
        let msgs = collect(mbox);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].starts_with(b"Subject: one"));
        assert!(msgs[1].starts_with(b"Subject: two"));
    }

    #[test]
    fn skips_garbage_before_first_separator() {
        let mbox = b"# This file was exported by some tool\n\
                     \n\
                     From a@example.com Wed Apr 30 10:00:00 2026\n\
                     Subject: real\n\
                     \n\
                     body\n";
        let msgs = collect(mbox);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].starts_with(b"Subject: real"));
    }

    #[test]
    fn unescapes_mboxrd_from_lines_in_body() {
        let mbox = b"From a@example.com Wed Apr 30 10:00:00 2026\n\
                     Subject: x\n\
                     \n\
                     >From the desk of\n\
                     >>From the assistant\n\
                     body\n";
        let msgs = collect(mbox);
        assert_eq!(msgs.len(), 1);
        let body = std::str::from_utf8(&msgs[0]).unwrap();
        assert!(body.contains("From the desk of"));
        assert!(body.contains(">From the assistant"));
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(collect(b"").is_empty());
        assert!(collect(b"no separator at all\n").is_empty());
    }

    #[test]
    fn is_mbox_file_matches_apple_mail_bundle() {
        // Apple Mail: `Inbox.mbox/mbox` — bare "mbox" inside `*.mbox/` dir.
        assert!(is_mbox_file(Path::new("/x/Inbox.mbox/mbox")));
        // Thunderbird / Takeout: `*.mbox`.
        assert!(is_mbox_file(Path::new("/x/All mail.mbox")));
        // A random bare `mbox` file outside an `.mbox` bundle: don't pick up.
        assert!(!is_mbox_file(Path::new("/x/mbox")));
        // Unrelated file: skip.
        assert!(!is_mbox_file(Path::new("/x/notes.txt")));
    }
}
