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

/// The MIME part chosen as the message's primary body — used when no PDF is
/// attached and the email *itself* is the receipt (Stripe/AWS-style HTML
/// receipts, body-only billing notices). `render-body` consumes the same
/// information later to render the body to PDF for 電帳法 archival.
#[derive(Debug, Clone)]
pub struct PrimaryBody {
    /// Depth-first preorder index of this part in the MIME tree (root = 0).
    /// Stable as long as the underlying RFC 822 bytes are unchanged.
    pub part_index: usize,
    /// `"text/html"` or `"text/plain"`.
    pub mime_type: String,
}

/// Pick the MIME part that should stand in as the receipt body when no PDF is
/// attached. Walks depth-first, ignores parts whose `Content-Disposition` is
/// `attachment`, and prefers `text/html` over `text/plain`. Returns `None`
/// when nothing usable is found (e.g. attachment-only messages, or bodies
/// that are neither text/html nor text/plain).
pub fn pick_primary_body_part(mail: &ParsedMail<'_>) -> Option<PrimaryBody> {
    let mut html: Option<PrimaryBody> = None;
    let mut plain: Option<PrimaryBody> = None;
    let mut counter: usize = 0;
    walk_for_body(mail, &mut counter, &mut html, &mut plain);
    html.or(plain)
}

fn walk_for_body(
    part: &ParsedMail<'_>,
    counter: &mut usize,
    html: &mut Option<PrimaryBody>,
    plain: &mut Option<PrimaryBody>,
) {
    let idx = *counter;
    *counter += 1;

    let is_attachment = matches!(
        part.get_content_disposition().disposition,
        mailparse::DispositionType::Attachment
    );
    let mime = part.ctype.mimetype.to_ascii_lowercase();

    if !is_attachment {
        if mime == "text/html" && html.is_none() {
            *html = Some(PrimaryBody {
                part_index: idx,
                mime_type: mime.clone(),
            });
        } else if mime == "text/plain" && plain.is_none() {
            *plain = Some(PrimaryBody {
                part_index: idx,
                mime_type: mime.clone(),
            });
        }
    }

    for sub in &part.subparts {
        walk_for_body(sub, counter, html, plain);
    }
}

/// Re-walk a parsed message and return the part at `target_index`. Used by
/// `render-body` to recover the body bytes after re-parsing the cached `.eml`.
/// Numbering matches [`pick_primary_body_part`] (depth-first preorder).
pub fn find_part_by_index<'a, 'b>(
    mail: &'a ParsedMail<'b>,
    target_index: usize,
) -> Option<&'a ParsedMail<'b>> {
    let mut counter: usize = 0;
    find_part_inner(mail, target_index, &mut counter)
}

fn find_part_inner<'a, 'b>(
    part: &'a ParsedMail<'b>,
    target_index: usize,
    counter: &mut usize,
) -> Option<&'a ParsedMail<'b>> {
    let idx = *counter;
    *counter += 1;
    if idx == target_index {
        return Some(part);
    }
    for sub in &part.subparts {
        if let Some(found) = find_part_inner(sub, target_index, counter) {
            return Some(found);
        }
    }
    None
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
    /// Where to write extracted PDFs (and, for body-primary messages, the
    /// materialised `.eml` when [`Self::eml_on_disk`] is `None`).
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
    /// Raw RFC 822 bytes of this message. Required so the body-primary path
    /// can either reference an on-disk `.eml` (eml/maildir) or materialise
    /// one in the cache (mbox, gmail) for `render-body` to read later.
    pub raw_bytes: &'a [u8],
    /// `Some(path)` when the source already keeps this message as a single
    /// `.eml` on disk (eml, maildir). `None` when the source can't (mbox
    /// concatenates many messages, gmail is remote) — in that case the
    /// body-primary path writes `<cache_dir>/<external_id>.eml` and uses it.
    pub eml_on_disk: Option<&'a Path>,
}

/// Apply `--since`, extract PDFs (or fall back to the body when there are
/// none), write artefacts to disk, and emit one Document per output. Returns
/// an empty Vec when the `Date:` header is older than `since`, or when there
/// are no PDFs *and* no usable body part.
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
    let to = parsed.headers.get_first_value("To");
    let message_id = parsed.headers.get_first_value("Message-ID");

    let mut pdfs = Vec::new();
    collect_pdf_attachments(parsed, &mut pdfs);

    if pdfs.is_empty() {
        return body_primary_record(parsed, opts, &subject, &from, &to, &date_str, &message_id);
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

/// Emit a single body-primary Document when the message has no PDF
/// attachments but does have a usable text/html or text/plain body. This is
/// the path that lets `render-body` later turn the email into a PDF for
/// 電帳法 archival (some receipts arrive purely in the body, no attachment).
fn body_primary_record(
    parsed: &ParsedMail<'_>,
    opts: &ProcessOpts<'_>,
    subject: &Option<String>,
    from: &Option<String>,
    to: &Option<String>,
    date_str: &Option<String>,
    message_id: &Option<String>,
) -> Result<Vec<Document>> {
    // Require at least one of the basic identifying headers before treating
    // a parse result as a real email. mailparse is lenient enough that random
    // binary garbage parses as a degenerate text/plain message — we don't
    // want such input to silently flow into the pipeline as a body-primary
    // "receipt".
    let has_real_headers =
        subject.is_some() || from.is_some() || date_str.is_some() || message_id.is_some();
    if !has_real_headers {
        if !opts.quiet {
            eprintln!(
                "{}: {}: no PDF attachment and no recognisable mail headers — skipping",
                opts.progress_tag, opts.progress_label
            );
        }
        return Ok(Vec::new());
    }

    let Some(body) = pick_primary_body_part(parsed) else {
        if !opts.quiet {
            eprintln!(
                "{}: {}: no PDF attachment and no usable body part — skipping",
                opts.progress_tag, opts.progress_label
            );
        }
        return Ok(Vec::new());
    };

    let external_id = match message_id.as_deref() {
        Some(id) => id
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string(),
        None => {
            let mut seeds: Vec<&[u8]> = opts.fallback_seeds.to_vec();
            seeds.push(b"body-primary");
            fallback_id(&seeds)
        }
    };

    // Resolve the .eml location: caller-provided on-disk path wins; otherwise
    // materialise a copy in the cache dir under a sanitised id.
    let eml_path: PathBuf = match opts.eml_on_disk {
        Some(p) => p.to_path_buf(),
        None => {
            let dest = opts
                .cache_dir
                .join(format!("{}.eml", sanitize_filename(&external_id)));
            std::fs::write(&dest, opts.raw_bytes)
                .with_context(|| format!("writing cached eml {}", dest.display()))?;
            dest
        }
    };

    if !opts.quiet {
        eprintln!(
            "{}: {} → body-primary ({}, eml={})",
            opts.progress_tag,
            opts.progress_label,
            body.mime_type,
            eml_path.display()
        );
    }

    let mut meta = json!({
        "subject": subject,
        "from": from,
        "to": to,
        "date": date_str,
        "body_is_primary": true,
        "body_part_index": body.part_index,
        "body_mime_type": body.mime_type,
        "eml_path": eml_path.to_string_lossy(),
    });
    if let Value::Object(ref mut m) = meta {
        for (k, v) in &opts.extra_meta {
            // Don't let source-provided extras silently overwrite the
            // canonical body-primary keys we just set above.
            m.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    Ok(vec![Document {
        source: opts.source.to_string(),
        external_id,
        attachment_path: None,
        source_meta: Some(meta),
        extracted: None,
        exported: None,
        status: "ok".to_string(),
    }])
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
    fn pick_primary_body_prefers_html_over_plain() {
        let raw = b"From: a@example.com\r\n\
                    Subject: Hi\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/alternative; boundary=BDY\r\n\
                    \r\n\
                    --BDY\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    plain version\r\n\
                    --BDY\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>html version</p>\r\n\
                    --BDY--\r\n";
        let parsed = parse_mail(raw).unwrap();
        let body = pick_primary_body_part(&parsed).expect("must find body");
        assert_eq!(body.mime_type, "text/html");
        // root=0, first leaf (plain)=1, second leaf (html)=2
        assert_eq!(body.part_index, 2);
        let part = find_part_by_index(&parsed, body.part_index).expect("re-find");
        assert_eq!(part.ctype.mimetype, "text/html");
    }

    #[test]
    fn pick_primary_body_falls_back_to_plain() {
        let raw = b"From: a@example.com\r\n\
                    Subject: Hi\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    just plain\r\n";
        let parsed = parse_mail(raw).unwrap();
        let body = pick_primary_body_part(&parsed).expect("must find body");
        assert_eq!(body.mime_type, "text/plain");
        assert_eq!(body.part_index, 0);
    }

    #[test]
    fn pick_primary_body_skips_attachment_disposition() {
        // An attached text/html (Content-Disposition: attachment) should not
        // be picked as the primary body — that's a literal HTML attachment,
        // not the receipt body.
        let raw = b"From: a@example.com\r\n\
                    Subject: Hi\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=BDY\r\n\
                    \r\n\
                    --BDY\r\n\
                    Content-Type: text/html\r\n\
                    Content-Disposition: attachment; filename=\"x.html\"\r\n\
                    \r\n\
                    <p>not the body</p>\r\n\
                    --BDY--\r\n";
        let parsed = parse_mail(raw).unwrap();
        assert!(pick_primary_body_part(&parsed).is_none());
    }

    #[test]
    fn pick_primary_body_returns_none_when_only_pdfs() {
        let raw = b"From: a@example.com\r\n\
                    Subject: Hi\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=BDY\r\n\
                    \r\n\
                    --BDY\r\n\
                    Content-Type: application/pdf\r\n\
                    Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    JVBERi0xLjQKJSVFT0YK\r\n\
                    --BDY--\r\n";
        let parsed = parse_mail(raw).unwrap();
        assert!(pick_primary_body_part(&parsed).is_none());
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
