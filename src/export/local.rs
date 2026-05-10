//! Local filesystem export.
//!
//! Writes the cached PDF to a structured location with a filename template
//! that satisfies the Japanese e-bookkeeping (電帳法) search requirement of
//! "transaction date / total amount / counterparty name". The defaults
//! produce a name like `2026-04-30_アクメ_12100円.pdf` — a single directory
//! listing already meets the search-by-three-fields requirement, so users
//! can drop the export root into Drive/Dropbox and be compliant.
//!
//! Reads [`Document`](crate::io::Document) JSONL on stdin. For each record
//! with both `attachment_path` and `extracted`, the file is copied to the
//! computed destination and an `exported.local` field is added on stdout.
//! Records missing those fields are passed through unchanged with
//! `status = "needs_review"` and a stderr warning, never aborting the run.

use crate::io::{Document, Extracted, read_jsonl_stdin, write_jsonl_stdout};
use anyhow::Context;
use clap::Args;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct LocalArgs {
    /// Root directory to write under. Created if it does not exist.
    #[arg(long)]
    pub root: String,

    /// Filename template. Placeholders:
    /// `{yyyy-mm-dd}` `{yyyy}` `{mm}` `{dd}` `{counterparty_name}`
    /// `{total_amount}` `{external_id}` `{source}`
    /// `{document_type}` (`invoice`/`receipt`/`other`)
    /// `{document_type_ja}` (`請求書`/`領収書`/`その他`).
    /// `{document_type*}` placeholders render to an empty string when classify
    /// did not set the document type — wrap them in template glue accordingly.
    /// May contain `/` to fan into subdirectories (e.g. `{yyyy}/{mm}/...`).
    #[arg(
        long,
        default_value = "{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf"
    )]
    pub name_template: String,

    /// Suppress per-row stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: LocalArgs) -> anyhow::Result<()> {
    let root = PathBuf::from(&args.root);
    std::fs::create_dir_all(&root).with_context(|| format!("creating root {}", args.root))?;

    let mut written = 0usize;
    let mut skipped = 0usize;

    for rec in read_jsonl_stdin::<Document>() {
        let mut doc = rec.context("reading Document JSONL on stdin")?;

        match plan_destination(&doc, &args.name_template, &root) {
            Ok((src, dest)) => {
                copy_atomic(&src, &dest)
                    .with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
                doc.exported = Some(merge_exported(doc.exported.take(), &dest));
                written += 1;
                if !args.quiet {
                    eprintln!("export local: {} → {}", doc.external_id, dest.display());
                }
            }
            Err(reason) => {
                doc.status = "needs_review".to_string();
                skipped += 1;
                if !args.quiet {
                    eprintln!("export local: skipped {} ({reason})", doc.external_id);
                }
            }
        }

        write_jsonl_stdout(&doc)?;
    }

    if !args.quiet {
        eprintln!("export local: wrote {written}, skipped {skipped}");
    }
    Ok(())
}

/// Build the destination path for a Document, or return a human-readable
/// reason explaining why this record cannot be exported.
fn plan_destination(
    doc: &Document,
    template: &str,
    root: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let src = doc
        .attachment_path
        .as_deref()
        .ok_or_else(|| "no attachment_path".to_string())?;
    let extracted = doc
        .extracted
        .as_ref()
        .ok_or_else(|| "no extracted fields (run classify first)".to_string())?;

    let name = render_template(template, doc, extracted);
    let dest = root.join(name);
    Ok((PathBuf::from(src), dest))
}

/// Substitute template placeholders. Unknown `{...}` tokens are left in place
/// rather than silently dropped, so a typo surfaces in the filename instead
/// of producing a cryptic `_.pdf`.
fn render_template(template: &str, doc: &Document, extracted: &Extracted) -> String {
    let date = extracted.transaction_date.as_str();
    let (yyyy, mm, dd) = split_iso_date(date);
    let amount = extracted.total_amount_jpy.to_string();
    let name = sanitize_filename_component(&extracted.counterparty_name);
    let doc_type_en = extracted.document_type.map(|t| t.en()).unwrap_or("");
    let doc_type_ja = extracted.document_type.map(|t| t.ja()).unwrap_or("");

    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unclosed `{` — emit verbatim and stop scanning.
            out.push('{');
            out.push_str(after);
            return out;
        };
        let key = &after[..close];
        let replacement = match key {
            "yyyy-mm-dd" => date,
            "yyyy" => yyyy,
            "mm" => mm,
            "dd" => dd,
            "counterparty_name" => name.as_str(),
            "total_amount" => amount.as_str(),
            "external_id" => doc.external_id.as_str(),
            "source" => doc.source.as_str(),
            "document_type" => doc_type_en,
            "document_type_ja" => doc_type_ja,
            _ => {
                // Unknown placeholder: re-emit literally.
                out.push('{');
                out.push_str(key);
                out.push('}');
                rest = &after[close + 1..];
                continue;
            }
        };
        out.push_str(replacement);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn split_iso_date(date: &str) -> (&str, &str, &str) {
    // Tolerate non-ISO input by returning whatever prefix is available; the
    // caller's own template still renders something rather than panicking.
    let mut parts = date.splitn(3, '-');
    let y = parts.next().unwrap_or("");
    let m = parts.next().unwrap_or("");
    let d = parts.next().unwrap_or("");
    (y, m, d)
}

/// Replace characters that are unsafe in filenames on any of the supported
/// platforms (macOS / Linux / Windows) with `_`. Slash is preserved when it
/// appears in the *template* (so `{yyyy}/{mm}` works as a directory path),
/// but never inside a substituted counterparty name — so we sanitize only
/// the substituted value, not the rendered output.
fn sanitize_filename_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Copy `src` to `dest` via a temp file + rename so an interrupted run never
/// leaves a half-written destination. Re-running with the same template
/// overwrites the previous output (the pipeline is idempotent by design).
fn copy_atomic(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = with_extension_suffix(dest, ".tmp");
    std::fs::copy(src, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} → {}", tmp.display(), dest.display()))?;
    Ok(())
}

fn with_extension_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn merge_exported(prev: Option<serde_json::Value>, dest: &Path) -> serde_json::Value {
    let mut obj = match prev {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(
        "local".into(),
        json!({ "path": dest.display().to_string() }),
    );
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{Document, DocumentType, Extracted};

    fn doc() -> Document {
        Document {
            source: "gmail".into(),
            external_id: "abc123".into(),
            attachment_path: Some("/tmp/in.pdf".into()),
            source_meta: None,
            extracted: Some(Extracted {
                transaction_date: "2026-04-30".into(),
                total_amount_jpy: 12100,
                counterparty_name: "アクメ商事".into(),
                counterparty_t_number: None,
                document_type: None,
                confidence: 0.9,
            }),
            exported: None,
            status: "ok".into(),
        }
    }

    #[test]
    fn default_template_renders_compliant_name() {
        let d = doc();
        let name = render_template(
            "{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf",
            &d,
            d.extracted.as_ref().unwrap(),
        );
        assert_eq!(name, "2026-04-30_アクメ商事_12100円.pdf");
    }

    #[test]
    fn date_components_split() {
        let d = doc();
        let name = render_template(
            "{yyyy}/{mm}/{dd}_{external_id}.pdf",
            &d,
            d.extracted.as_ref().unwrap(),
        );
        assert_eq!(name, "2026/04/30_abc123.pdf");
    }

    #[test]
    fn slashes_in_counterparty_are_sanitized() {
        let mut d = doc();
        d.extracted.as_mut().unwrap().counterparty_name = "A/B:C*D".into();
        let name = render_template("{counterparty_name}.pdf", &d, d.extracted.as_ref().unwrap());
        assert_eq!(name, "A_B_C_D.pdf");
    }

    #[test]
    fn document_type_placeholders_render_jp_and_en() {
        let mut d = doc();
        d.extracted.as_mut().unwrap().document_type = Some(DocumentType::Receipt);
        let name = render_template(
            "{yyyy-mm-dd}_{counterparty_name}_{total_amount}円_{document_type_ja}.pdf",
            &d,
            d.extracted.as_ref().unwrap(),
        );
        assert_eq!(name, "2026-04-30_アクメ商事_12100円_領収書.pdf");

        let name_en = render_template(
            "{yyyy-mm-dd}_{document_type}_{external_id}.pdf",
            &d,
            d.extracted.as_ref().unwrap(),
        );
        assert_eq!(name_en, "2026-04-30_receipt_abc123.pdf");
    }

    #[test]
    fn document_type_placeholder_empty_when_unknown() {
        // None → render to empty string so the rest of the template still
        // produces something usable rather than `{document_type_ja}` lingering.
        let d = doc();
        let name = render_template(
            "{yyyy-mm-dd}_{counterparty_name}_{document_type_ja}.pdf",
            &d,
            d.extracted.as_ref().unwrap(),
        );
        assert_eq!(name, "2026-04-30_アクメ商事_.pdf");
    }

    #[test]
    fn unknown_placeholder_passes_through() {
        let d = doc();
        let name = render_template("{yyyy}_{nope}.pdf", &d, d.extracted.as_ref().unwrap());
        assert_eq!(name, "2026_{nope}.pdf");
    }

    #[test]
    fn missing_extracted_yields_skip_reason() {
        let mut d = doc();
        d.extracted = None;
        let err = plan_destination(&d, "x.pdf", Path::new("/r")).unwrap_err();
        assert!(err.contains("classify"), "got: {err}");
    }

    #[test]
    fn missing_attachment_yields_skip_reason() {
        let mut d = doc();
        d.attachment_path = None;
        let err = plan_destination(&d, "x.pdf", Path::new("/r")).unwrap_err();
        assert!(err.contains("attachment_path"), "got: {err}");
    }
}
