//! `fetch eml` — pull PDF attachments out of locally-stored `.eml` files.
//!
//! Walks `--dir` recursively, parses each `.eml` with `mailparse`, writes any
//! PDF attachments into a cache directory, and emits one Document JSONL record
//! per attachment on stdout. Unreadable files are skipped with a stderr
//! warning so a single broken message never aborts the whole run.

use crate::io::{Document, write_jsonl_stdout};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Args;
use mailparse::{MailHeaderMap, ParsedMail, dateparse, parse_mail};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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
        None => default_cache_dir()?,
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
                    eprintln!("fetch eml: walk error: {e}");
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
                    if let Some(limit) = args.limit {
                        if emitted >= limit {
                            if !args.quiet {
                                eprintln!("fetch eml: reached --limit {limit}");
                            }
                            break 'outer;
                        }
                    }
                }
            }
            Err(e) => {
                if !args.quiet {
                    eprintln!("fetch eml: skipping {}: {e:#}", path.display());
                }
            }
        }
    }

    if !args.quiet {
        eprintln!("fetch eml: emitted {emitted} document(s)");
    }
    Ok(())
}

fn is_eml(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("eml"))
        .unwrap_or(false)
}

/// Per-OS cache root, resolved with `std::env` only:
/// - macOS:   `$HOME/Library/Caches`
/// - Windows: `%LOCALAPPDATA%` (falls back to `%APPDATA%`)
/// - Other:   `$XDG_CACHE_HOME` or `$HOME/.cache`
fn default_cache_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join("Library").join("Caches"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    let base = base.ok_or_else(|| {
        anyhow::anyhow!("could not resolve a default cache directory; pass --cache-dir")
    })?;
    Ok(base.join("fetchdoc").join("eml-attachments"))
}

fn process_file(
    path: &Path,
    cache_dir: &Path,
    since: Option<NaiveDate>,
    quiet: bool,
) -> Result<Vec<Document>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_mail(&raw).context("parsing MIME message")?;

    let date_str = parsed.headers.get_first_value("Date");
    if let (Some(since_d), Some(ds)) = (since, date_str.as_deref())
        && let Some(msg_date) = parse_header_date(ds)
        && msg_date < since_d
    {
        return Ok(Vec::new());
    }

    let subject = parsed.headers.get_first_value("Subject");
    let from = parsed.headers.get_first_value("From");
    let message_id = parsed.headers.get_first_value("Message-ID");

    let mut pdfs = Vec::new();
    collect_pdf_attachments(&parsed, &mut pdfs);

    if pdfs.is_empty() {
        if !quiet {
            eprintln!("fetch eml: {}: no PDF attachments", path.display());
        }
        return Ok(Vec::new());
    }

    let mut docs = Vec::with_capacity(pdfs.len());
    for att in pdfs {
        let external_id = match message_id.as_deref() {
            Some(id) => id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string(),
            None => fallback_id(path, &att.filename),
        };

        let dest_name = sanitize_filename(&format!(
            "{}_{}",
            sanitize_filename(&external_id),
            sanitize_filename(&att.filename)
        ));
        let dest = cache_dir.join(&dest_name);
        std::fs::write(&dest, &att.body).with_context(|| format!("writing {}", dest.display()))?;

        if !quiet {
            eprintln!("fetch eml: {} → {}", path.display(), dest.display());
        }

        let source_meta = json!({
            "subject": subject,
            "from": from,
            "date": date_str,
            "eml_path": path.to_string_lossy(),
            "attachment_filename": att.filename,
        });

        docs.push(Document {
            source: "eml".to_string(),
            external_id,
            attachment_path: Some(dest.to_string_lossy().into_owned()),
            source_meta: Some(source_meta),
            extracted: None,
            exported: None,
            status: "ok".to_string(),
        });
    }
    Ok(docs)
}

struct PdfAttachment {
    filename: String,
    body: Vec<u8>,
}

fn collect_pdf_attachments(part: &ParsedMail<'_>, out: &mut Vec<PdfAttachment>) {
    let mime = part.ctype.mimetype.to_ascii_lowercase();
    let filename = part
        .get_content_disposition()
        .params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned());

    let is_pdf = mime == "application/pdf"
        || filename
            .as_deref()
            .map(|n| n.to_ascii_lowercase().ends_with(".pdf"))
            .unwrap_or(false);

    if is_pdf {
        if let Ok(body) = part.get_body_raw() {
            let name = filename.unwrap_or_else(|| "attachment.pdf".to_string());
            out.push(PdfAttachment {
                filename: name,
                body,
            });
        }
        return;
    }

    for sub in &part.subparts {
        collect_pdf_attachments(sub, out);
    }
}

/// Parse a `Date:` header into a `NaiveDate`. Tries `mailparse::dateparse`
/// first (lenient — tolerates wrong day-of-week, missing seconds, etc.) and
/// falls back to chrono's strict RFC 2822 parser.
fn parse_header_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(ts) = dateparse(s) {
        return DateTime::<Utc>::from_timestamp(ts, 0).map(|d| d.date_naive());
    }
    DateTime::parse_from_rfc2822(s).ok().map(|d| d.date_naive())
}

/// 16-hex-char fingerprint used when the message has no `Message-ID`.
fn fallback_id(path: &Path, attachment_filename: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(attachment_filename.as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Strip path separators / control chars and clamp length so the cache
/// filename is safe on every OS we ship to.
fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s = s
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();
    if s.is_empty() {
        s = "attachment".to_string();
    }
    if s.len() > 200 {
        s.truncate(200);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_id_is_stable_and_16_hex() {
        let id = fallback_id(Path::new("/tmp/a.eml"), "invoice.pdf");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let again = fallback_id(Path::new("/tmp/a.eml"), "invoice.pdf");
        assert_eq!(id, again);

        let other = fallback_id(Path::new("/tmp/b.eml"), "invoice.pdf");
        assert_ne!(id, other);
    }

    #[test]
    fn sanitize_strips_path_chars() {
        // Path separators and other unsafe chars become `_`; dots in the
        // middle are left alone (they're fine in filenames).
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename("a\\b/c:d"), "a_b_c_d");
        assert_eq!(sanitize_filename("..."), "attachment");
        assert_eq!(sanitize_filename("ok.pdf"), "ok.pdf");
    }

    #[test]
    fn parse_header_date_handles_typical_headers() {
        // Real-world Date headers often have a wrong day-of-week or skip it
        // entirely; the parser should accept all three forms.
        let d = parse_header_date("Thu, 30 Apr 2026 10:00:00 +0900").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
        assert!(parse_header_date("30 Apr 2026 10:00:00 +0900").is_some());
        // Wrong day-of-week (Apr 30 2026 is a Thursday, not Wed): still parses.
        assert!(parse_header_date("Wed, 30 Apr 2026 10:00:00 +0900").is_some());
    }

    #[test]
    fn collect_pdf_attachments_walks_multipart() {
        let raw = b"From: a@example.com\r\n\
                    To: b@example.com\r\n\
                    Subject: Test\r\n\
                    Date: Wed, 30 Apr 2026 10:00:00 +0900\r\n\
                    Message-ID: <abc@example.com>\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=BDY\r\n\
                    \r\n\
                    --BDY\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    hello\r\n\
                    --BDY\r\n\
                    Content-Type: application/pdf; name=\"invoice.pdf\"\r\n\
                    Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    JVBERi0xLjQKJSVFT0YK\r\n\
                    --BDY--\r\n";
        let parsed = parse_mail(raw).unwrap();
        let mut pdfs = Vec::new();
        collect_pdf_attachments(&parsed, &mut pdfs);
        assert_eq!(pdfs.len(), 1);
        assert_eq!(pdfs[0].filename, "invoice.pdf");
        assert!(pdfs[0].body.starts_with(b"%PDF-1.4"));
    }
}
