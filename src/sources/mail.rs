//! Helpers shared between mail-based sources (`fetch eml`, `fetch mbox`).
//!
//! Everything here is format-agnostic: pure functions over a single parsed
//! message, plus filesystem utilities (cache dir, filename sanitisation).
//! Per-source code (`eml.rs`, `mbox.rs`) handles file/stream layout and
//! drives this module per message.

use crate::io::Document;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use mailparse::{MailHeaderMap, ParsedMail, dateparse};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// One PDF attachment extracted from a MIME message.
pub struct PdfAttachment {
    pub filename: String,
    pub body: Vec<u8>,
}

/// Walk a MIME tree and collect every PDF-shaped leaf (by `Content-Type:
/// application/pdf` *or* by `.pdf` filename — some senders mislabel the
/// content type). Once a leaf is taken as a PDF we stop recursing into it.
pub fn collect_pdf_attachments(part: &ParsedMail<'_>, out: &mut Vec<PdfAttachment>) {
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
pub fn parse_header_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(ts) = dateparse(s) {
        return DateTime::<Utc>::from_timestamp(ts, 0).map(|d| d.date_naive());
    }
    DateTime::parse_from_rfc2822(s).ok().map(|d| d.date_naive())
}

/// 16-hex-char fingerprint used when the message has no `Message-ID`.
/// Each `seed` segment is fed to the hasher with a `\0` separator so two
/// different combinations cannot collide via concatenation.
pub fn fallback_id(seeds: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for (i, s) in seeds.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\0");
        }
        hasher.update(s);
    }
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
pub fn sanitize_filename(name: &str) -> String {
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

/// Per-OS cache root, resolved with `std::env` only:
/// - macOS:   `$HOME/Library/Caches`
/// - Windows: `%LOCALAPPDATA%` (falls back to `%APPDATA%`)
/// - Other:   `$XDG_CACHE_HOME` or `$HOME/.cache`
///
/// `subdir` is appended under `fetchdoc/` (e.g. `"eml-attachments"`).
pub fn default_cache_dir(subdir: &str) -> Result<PathBuf> {
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
    Ok(base.join("fetchdoc").join(subdir))
}

/// Inputs passed to [`process_parsed_message`] that the caller (per-source
/// driver) is in a better position to decide than this module.
pub struct ProcessOpts<'a> {
    /// `"eml"`, `"mbox"`, etc. Goes into `Document.source`.
    pub source: &'a str,
    /// Where to write extracted PDFs.
    pub cache_dir: &'a Path,
    /// Drop the message if its `Date:` header is older than this.
    pub since: Option<NaiveDate>,
    /// Used when `Message-ID` is missing — keep distinct entries distinct.
    /// Each segment is fed to sha256 with a NUL separator.
    pub fallback_seeds: &'a [&'a [u8]],
    /// Extra fields merged into `source_meta` (e.g. `eml_path`, `mbox_path`).
    pub extra_meta: Map<String, Value>,
    /// Tag used in stderr progress lines (e.g. `"fetch mbox"`).
    pub progress_tag: &'a str,
    /// Human-readable label of where this message came from (path or
    /// `path#index`). Used in stderr lines.
    pub progress_label: &'a str,
    /// If true, skip the per-message stderr lines.
    pub quiet: bool,
}

/// Apply `--since`, extract PDFs, write them to disk, and emit one Document
/// per attachment. Returns an empty Vec when the message has no PDFs or the
/// `Date:` header is older than `since`.
pub fn process_parsed_message(
    parsed: &ParsedMail<'_>,
    opts: &ProcessOpts<'_>,
) -> Result<Vec<Document>> {
    let date_str = parsed.headers.get_first_value("Date");
    if let (Some(since_d), Some(ds)) = (opts.since, date_str.as_deref())
        && let Some(msg_date) = parse_header_date(ds)
        && msg_date < since_d
    {
        return Ok(Vec::new());
    }

    let subject = parsed.headers.get_first_value("Subject");
    let from = parsed.headers.get_first_value("From");
    let message_id = parsed.headers.get_first_value("Message-ID");

    let mut pdfs = Vec::new();
    collect_pdf_attachments(parsed, &mut pdfs);
    if pdfs.is_empty() {
        if !opts.quiet {
            eprintln!(
                "{}: {}: no PDF attachments",
                opts.progress_tag, opts.progress_label
            );
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
            None => {
                let mut seeds: Vec<&[u8]> = opts.fallback_seeds.to_vec();
                seeds.push(att.filename.as_bytes());
                fallback_id(&seeds)
            }
        };

        let dest_name = sanitize_filename(&format!(
            "{}_{}",
            sanitize_filename(&external_id),
            sanitize_filename(&att.filename)
        ));
        let dest = opts.cache_dir.join(&dest_name);
        std::fs::write(&dest, &att.body).with_context(|| format!("writing {}", dest.display()))?;

        if !opts.quiet {
            eprintln!(
                "{}: {} → {}",
                opts.progress_tag,
                opts.progress_label,
                dest.display()
            );
        }

        let mut meta = json!({
            "subject": subject,
            "from": from,
            "date": date_str,
            "attachment_filename": att.filename,
        });
        if let Value::Object(ref mut m) = meta {
            for (k, v) in &opts.extra_meta {
                m.insert(k.clone(), v.clone());
            }
        }

        docs.push(Document {
            source: opts.source.to_string(),
            external_id,
            attachment_path: Some(dest.to_string_lossy().into_owned()),
            source_meta: Some(meta),
            extracted: None,
            exported: None,
            status: "ok".to_string(),
        });
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailparse::parse_mail;

    #[test]
    fn fallback_id_is_stable_and_16_hex() {
        let id = fallback_id(&[b"/tmp/a.eml", b"invoice.pdf"]);
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let again = fallback_id(&[b"/tmp/a.eml", b"invoice.pdf"]);
        assert_eq!(id, again);
        let other = fallback_id(&[b"/tmp/b.eml", b"invoice.pdf"]);
        assert_ne!(id, other);
        // The NUL separator prevents trivial concatenation collisions.
        assert_ne!(fallback_id(&[b"ab", b"cd"]), fallback_id(&[b"abc", b"d"]));
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename("a\\b/c:d"), "a_b_c_d");
        assert_eq!(sanitize_filename("..."), "attachment");
        assert_eq!(sanitize_filename("ok.pdf"), "ok.pdf");
    }

    #[test]
    fn parse_header_date_handles_typical_headers() {
        let d = parse_header_date("Thu, 30 Apr 2026 10:00:00 +0900").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
        assert!(parse_header_date("30 Apr 2026 10:00:00 +0900").is_some());
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
