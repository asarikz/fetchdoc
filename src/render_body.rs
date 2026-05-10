//! `fetchdoc render-body` — turn body-primary email records into PDFs.
//!
//! Reads Document JSONL from stdin. Records that already have an
//! `attachment_path` (the normal PDF case) are passed through unchanged.
//! Records flagged with `source_meta.body_is_primary == true` are re-parsed
//! from their cached `.eml`, the chosen MIME body part is wrapped in a small
//! HTML envelope (with a header carrying From / To / Subject / Date so the
//! rendered PDF stands alone as audit evidence), and an external HTML→PDF
//! renderer is invoked. The Document is then emitted with `attachment_path`
//! pointing at the new PDF.
//!
//! Renderer discovery (in order):
//!   1. Chromium-family: `chromium`, `chromium-browser`, `google-chrome`,
//!      `chrome` — invoked headless with `--print-to-pdf`.
//!   2. `weasyprint` — pure-Python, faithful with CSS, no JS.
//!   3. `wkhtmltopdf` — last resort. Deprecated upstream; we warn but use it
//!      if it's the only thing available.
//!
//! `--renderer` overrides discovery. If nothing is found, render-body errors
//! out with installation hints rather than silently passing the record on.
//!
//! Tests set `FETCHDOC_RENDER_BODY_FAKE=1` to short-circuit external
//! invocation and write a stub PDF, so CI doesn't need a real renderer.

use crate::io::{Document, read_jsonl_stdin, write_jsonl_stdout};
use crate::sources::mail;
use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use mailparse::parse_mail;
use serde_json::Value;
use std::path::{Path, PathBuf};

const FAKE_PDF_ENV: &str = "FETCHDOC_RENDER_BODY_FAKE";
const FAKE_PDF_BYTES: &[u8] = b"%PDF-1.4\n% fake fetchdoc render-body stub\n%%EOF\n";
const PROGRESS_TAG: &str = "render-body";

#[derive(Args, Debug)]
pub struct RenderBodyArgs {
    /// Directory to write rendered PDFs. Defaults to
    /// `<os-cache>/fetchdoc/body-pdfs/`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Which renderer to use. `auto` searches `$PATH` in the order
    /// chromium → weasyprint → wkhtmltopdf.
    #[arg(long, value_enum, default_value_t = RendererChoice::Auto)]
    pub renderer: RendererChoice,

    /// Suppress per-record stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum RendererChoice {
    Auto,
    Chromium,
    Weasyprint,
    Wkhtmltopdf,
}

#[derive(Debug, Clone)]
enum Renderer {
    Chromium(String),
    Weasyprint(String),
    Wkhtmltopdf(String),
    /// Test-only: write [`FAKE_PDF_BYTES`] without invoking anything.
    Fake,
}

pub async fn run(args: RenderBodyArgs) -> Result<()> {
    let cache_dir = match args.cache_dir {
        Some(p) => p,
        None => mail::default_cache_dir("body-pdfs")?,
    };
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let renderer = if std::env::var_os(FAKE_PDF_ENV).is_some() {
        Renderer::Fake
    } else {
        resolve_renderer(args.renderer)?
    };
    if !args.quiet {
        eprintln!("{PROGRESS_TAG}: using renderer {}", renderer.label());
    }

    for line_res in read_jsonl_stdin::<Document>() {
        let mut doc = line_res?;
        if doc.attachment_path.is_some() {
            // Already a real PDF (or some other artefact a previous stage
            // produced). Don't re-render — just pass it through.
            write_jsonl_stdout(&doc)?;
            continue;
        }
        if !is_body_primary(&doc) {
            // No PDF and not flagged as body-primary either: nothing for us
            // to do, but don't drop it on the floor — the user piped it here.
            if !args.quiet {
                eprintln!(
                    "{PROGRESS_TAG}: {}: no attachment_path and not body_is_primary — passing through",
                    doc.external_id
                );
            }
            write_jsonl_stdout(&doc)?;
            continue;
        }

        match render_one(&mut doc, &renderer, &cache_dir, args.quiet) {
            Ok(()) => write_jsonl_stdout(&doc)?,
            Err(e) => {
                // A failed render shouldn't kill the whole pipeline — flag
                // the record for review and emit it so downstream stages see
                // the failure (and the user can re-run later).
                if !args.quiet {
                    eprintln!("{PROGRESS_TAG}: {}: render failed: {e:#}", doc.external_id);
                }
                doc.status = "needs_review".to_string();
                if let Some(Value::Object(ref mut m)) = doc.source_meta {
                    m.insert(
                        "render_body_error".to_string(),
                        Value::String(format!("{e:#}")),
                    );
                }
                write_jsonl_stdout(&doc)?;
            }
        }
    }
    Ok(())
}

fn is_body_primary(doc: &Document) -> bool {
    doc.source_meta
        .as_ref()
        .and_then(|m| m.get("body_is_primary"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn render_one(
    doc: &mut Document,
    renderer: &Renderer,
    cache_dir: &Path,
    quiet: bool,
) -> Result<()> {
    let meta = doc
        .source_meta
        .as_ref()
        .ok_or_else(|| anyhow!("body-primary record missing source_meta"))?;
    let eml_path = meta
        .get("eml_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("body-primary record missing source_meta.eml_path"))?;
    let part_index = meta
        .get("body_part_index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("body-primary record missing source_meta.body_part_index"))?
        as usize;

    let raw = std::fs::read(eml_path).with_context(|| format!("reading {eml_path}"))?;
    let parsed = parse_mail(&raw).context("parsing cached eml")?;
    let part = mail::find_part_by_index(&parsed, part_index)
        .ok_or_else(|| anyhow!("body part index {part_index} not found in {eml_path}"))?;

    let html = build_html_envelope(part, meta)?;

    let html_path = cache_dir.join(format!(
        "{}.html",
        mail::sanitize_filename(&doc.external_id)
    ));
    let pdf_path = cache_dir.join(format!("{}.pdf", mail::sanitize_filename(&doc.external_id)));
    std::fs::write(&html_path, &html)
        .with_context(|| format!("writing {}", html_path.display()))?;

    invoke_renderer(renderer, &html_path, &pdf_path)?;

    if !quiet {
        eprintln!(
            "{PROGRESS_TAG}: {} → {}",
            doc.external_id,
            pdf_path.display()
        );
    }
    doc.attachment_path = Some(pdf_path.to_string_lossy().into_owned());
    if let Some(Value::Object(ref mut m)) = doc.source_meta {
        m.insert(
            "body_html_path".to_string(),
            Value::String(html_path.to_string_lossy().into_owned()),
        );
    }
    Ok(())
}

/// Wrap the chosen MIME part in a self-contained HTML document: a small
/// header summarising the message metadata (so the resulting PDF stands on
/// its own as 電帳法 evidence) followed by the original body. For text/plain
/// we HTML-escape and wrap in `<pre>`. For text/html we don't try to extract
/// just the `<body>` — most renderers tolerate our header sitting above an
/// `<html>` document, and stripping wrappers correctly would need a real
/// HTML parser dependency.
fn build_html_envelope(part: &mailparse::ParsedMail<'_>, meta: &Value) -> Result<String> {
    let mime = part.ctype.mimetype.to_ascii_lowercase();
    let decoded = part.get_body().context("decoding body part")?;

    let body_html = match mime.as_str() {
        "text/html" => decoded,
        "text/plain" => format!(
            "<pre style=\"white-space: pre-wrap; word-wrap: break-word; font-family: monospace;\">{}</pre>",
            html_escape(&decoded)
        ),
        other => anyhow::bail!("unsupported body mime type {other}"),
    };

    let header = format_header(meta);
    Ok(format!(
        "<!DOCTYPE html>\n\
         <html><head><meta charset=\"utf-8\"><title>fetchdoc render-body</title></head>\n\
         <body style=\"font-family: -apple-system, system-ui, sans-serif;\">\n\
         {header}\n\
         <hr/>\n\
         <article>\n{body_html}\n</article>\n\
         </body></html>\n"
    ))
}

fn format_header(meta: &Value) -> String {
    let g = |k: &str| -> String {
        meta.get(k)
            .and_then(|v| v.as_str())
            .map(html_escape)
            .unwrap_or_default()
    };
    format!(
        "<header style=\"font-size: 11px; color: #555; margin-bottom: 8px;\">\
         <table>\
         <tr><td><b>From</b></td><td>{from}</td></tr>\
         <tr><td><b>To</b></td><td>{to}</td></tr>\
         <tr><td><b>Date</b></td><td>{date}</td></tr>\
         <tr><td><b>Subject</b></td><td>{subject}</td></tr>\
         </table></header>",
        from = g("from"),
        to = g("to"),
        date = g("date"),
        subject = g("subject"),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn invoke_renderer(renderer: &Renderer, input: &Path, output: &Path) -> Result<()> {
    if matches!(renderer, Renderer::Fake) {
        std::fs::write(output, FAKE_PDF_BYTES)
            .with_context(|| format!("writing fake pdf {}", output.display()))?;
        return Ok(());
    }

    let mut cmd = match renderer {
        Renderer::Chromium(bin) => {
            let mut c = std::process::Command::new(bin);
            c.arg("--headless")
                .arg("--disable-gpu")
                .arg("--no-sandbox")
                .arg(format!("--print-to-pdf={}", output.display()))
                .arg(format!("file://{}", input.display()));
            c
        }
        Renderer::Weasyprint(bin) => {
            let mut c = std::process::Command::new(bin);
            c.arg(input).arg(output);
            c
        }
        Renderer::Wkhtmltopdf(bin) => {
            let mut c = std::process::Command::new(bin);
            c.arg("--quiet").arg(input).arg(output);
            c
        }
        Renderer::Fake => unreachable!("handled above"),
    };
    let status = cmd
        .status()
        .with_context(|| format!("spawning renderer {}", renderer.label()))?;
    if !status.success() {
        anyhow::bail!("renderer {} exited with {status}", renderer.label());
    }
    if !output.exists() {
        anyhow::bail!(
            "renderer {} returned success but did not write {}",
            renderer.label(),
            output.display()
        );
    }
    Ok(())
}

impl Renderer {
    fn label(&self) -> String {
        match self {
            Renderer::Chromium(b) => format!("chromium ({b})"),
            Renderer::Weasyprint(b) => format!("weasyprint ({b})"),
            Renderer::Wkhtmltopdf(b) => format!("wkhtmltopdf ({b})"),
            Renderer::Fake => "fake (test stub)".to_string(),
        }
    }
}

fn resolve_renderer(choice: RendererChoice) -> Result<Renderer> {
    match choice {
        RendererChoice::Auto => discover_renderer().ok_or_else(|| {
            anyhow!(
                "no HTML→PDF renderer found on PATH. Install one of: \
                 chromium / google-chrome (recommended), weasyprint, or wkhtmltopdf. \
                 You can also pass --renderer to force a specific one."
            )
        }),
        RendererChoice::Chromium => {
            find_in_path(&["chromium", "chromium-browser", "google-chrome", "chrome"])
                .map(Renderer::Chromium)
                .ok_or_else(|| {
                    anyhow!("--renderer chromium specified but no chromium binary on PATH")
                })
        }
        RendererChoice::Weasyprint => find_in_path(&["weasyprint"])
            .map(Renderer::Weasyprint)
            .ok_or_else(|| anyhow!("--renderer weasyprint specified but not on PATH")),
        RendererChoice::Wkhtmltopdf => {
            eprintln!(
                "{PROGRESS_TAG}: warning: wkhtmltopdf is deprecated upstream; consider chromium or weasyprint"
            );
            find_in_path(&["wkhtmltopdf"])
                .map(Renderer::Wkhtmltopdf)
                .ok_or_else(|| anyhow!("--renderer wkhtmltopdf specified but not on PATH"))
        }
    }
}

fn discover_renderer() -> Option<Renderer> {
    if let Some(b) = find_in_path(&["chromium", "chromium-browser", "google-chrome", "chrome"]) {
        return Some(Renderer::Chromium(b));
    }
    if let Some(b) = find_in_path(&["weasyprint"]) {
        return Some(Renderer::Weasyprint(b));
    }
    if let Some(b) = find_in_path(&["wkhtmltopdf"]) {
        eprintln!(
            "{PROGRESS_TAG}: warning: only wkhtmltopdf was found and it's deprecated upstream"
        );
        return Some(Renderer::Wkhtmltopdf(b));
    }
    None
}

/// Look up `names` in `$PATH` in order; return the first match. We do this
/// by hand rather than pulling in the `which` crate to keep our dep tree
/// minimal (single-binary distribution + cargo install is the priority).
fn find_in_path(names: &[&str]) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
            // On Windows, also try common executable extensions.
            if cfg!(windows) {
                for ext in [".exe", ".bat", ".cmd"] {
                    let with_ext = dir.join(format!("{name}{ext}"));
                    if is_executable(&with_ext) {
                        return Some(with_ext.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn html_escape_handles_quotes_and_angles() {
        assert_eq!(
            html_escape("<a href=\"x&y\">'hi'</a>"),
            "&lt;a href=&quot;x&amp;y&quot;&gt;&#39;hi&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn format_header_renders_present_fields_and_skips_missing() {
        let meta = json!({"from": "a@x", "subject": "<inv>"});
        let h = format_header(&meta);
        assert!(h.contains("a@x"));
        // `<inv>` must be HTML-escaped.
        assert!(h.contains("&lt;inv&gt;"));
        // `to` and `date` were absent — should render empty cells, not panic.
        assert!(h.contains("<b>To</b>"));
    }

    #[test]
    fn build_html_wraps_text_plain_in_pre_and_escapes() {
        let raw = b"From: a@example.com\r\n\
                    Subject: hi\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    if a < b then &c\r\n";
        let parsed = parse_mail(raw).unwrap();
        let meta = json!({"from": "a@example.com", "subject": "hi"});
        let html = build_html_envelope(&parsed, &meta).unwrap();
        assert!(html.contains("<pre"));
        assert!(html.contains("if a &lt; b then &amp;c"));
    }

    #[test]
    fn build_html_passes_text_html_through() {
        let raw = b"From: a@example.com\r\n\
                    Subject: hi\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>hello <b>world</b></p>\r\n";
        let parsed = parse_mail(raw).unwrap();
        let meta = json!({"from": "a@example.com", "subject": "hi"});
        let html = build_html_envelope(&parsed, &meta).unwrap();
        assert!(html.contains("<p>hello <b>world</b></p>"));
    }
}
