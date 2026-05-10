//! `import csv --infer` — generate a profile by handing the file head to
//! Anthropic, save it under `~/.config/fetchdoc/profiles/<name>.toml`, and
//! continue with the deterministic CSV parser.
//!
//! Design notes:
//! - We send the **first ~50 lines** of the file (capped at 16 KB) — enough
//!   to disambiguate column meaning, small enough to keep this a single
//!   cheap call. **Per-row data is never sent on subsequent runs**: the
//!   deterministic profile takes over.
//! - Encoding is sniffed locally first (UTF-8 → fall back to Shift_JIS) so
//!   we can decode the head before showing it to the model. The model may
//!   override the `encoding` field in the TOML if it disagrees.
//! - The TOML is validated via [`Profile::from_toml_str`] before being saved
//!   or used, so a hallucinated or off-spec response fails fast.

use super::Profile;
use super::csv::CsvArgs;
use anyhow::Context;
use std::path::{Path, PathBuf};

const HEAD_BYTES: usize = 16 * 1024;
const HEAD_LINES: usize = 50;
const MAX_TOKENS: u32 = 1024;

pub async fn run_csv(args: &CsvArgs) -> anyhow::Result<()> {
    if args.input == "-" {
        anyhow::bail!(
            "import csv --infer requires a file path; profile generation needs to peek \
             at the head of a seekable file (stdin would be consumed)."
        );
    }

    let bytes = super::csv::read_input_bytes(&args.input)?;
    let (head_text, sniffed_encoding) = sniff_head(&bytes);
    let name = profile_name(args.name.as_deref(), &args.input)?;

    let save_path = save_path_for(&name)?;
    if save_path.exists() {
        anyhow::bail!(
            "profile {} already exists at {}. Pass `--name <other>` or delete the existing file.",
            name,
            save_path.display()
        );
    }

    if !args.quiet {
        eprintln!(
            "infer: sniffed encoding = {sniffed_encoding}, head = {} lines, asking Anthropic…",
            head_text.lines().count()
        );
    }

    let client = crate::anthropic::Client::from_env()?;
    let user_prompt = build_user_prompt(&name, sniffed_encoding, &head_text);
    let raw = client
        .complete(SYSTEM_PROMPT, &user_prompt, MAX_TOKENS)
        .await
        .context("calling Anthropic Messages API")?;

    let toml_text = strip_code_fence(&raw);
    let profile = Profile::from_toml_str(toml_text)
        .with_context(|| format!("validating inferred profile:\n{toml_text}"))?;

    write_profile(&save_path, toml_text)?;
    if !args.quiet {
        eprintln!(
            "infer: wrote profile to {} (model {})",
            save_path.display(),
            client.model()
        );
    }

    super::csv::run_with_bytes(&bytes, &profile, &args.input, args.quiet)
}

/// System message: tell the model exactly which TOML schema to emit. Keeping
/// the schema inline (vs. handing it the rust source) keeps the prompt small
/// and stable across refactors.
const SYSTEM_PROMPT: &str = r#"You generate fetchdoc CSV import profiles.

Your output MUST be a single TOML document and nothing else (no prose, no markdown fence).

The TOML schema is:

  name        = string                      # short identifier, lowercase, no spaces
  encoding    = "utf-8" | "shift_jis" | "cp932"
  delimiter   = ","   |  "\t"
  header_row  = integer (1-indexed row of the header line; defaults to 1)
  skip_rows   = integer (data rows to skip after the header; defaults to 0)
  date_format = chrono strftime pattern (e.g. "%Y/%m/%d", "%Y-%m-%d")

  [columns]
  posted_date = "<header text>"             # required
  description = "<header text>"             # required
  # exactly ONE of:
  amount      = "<header text>"             # signed: outflow negative, inflow positive
  # or BOTH of:
  withdrawal  = "<header text>"             # outflow magnitude (positive in source)
  deposit     = "<header text>"             # inflow magnitude (positive in source)
  # optional:
  balance     = "<header text>"
  memo        = "<header text>"
  value_date  = "<header text>"

Rules:
- Match column names EXACTLY as they appear in the header line (full-width chars, spaces, etc.).
- Pick `amount` for single-column signed amounts; otherwise use `withdrawal`+`deposit`.
- Set `header_row` to the line number where the actual header lives if there are
  comment/banner lines above it.
- Use the `name` value provided by the user verbatim.
"#;

fn build_user_prompt(name: &str, encoding: &str, head: &str) -> String {
    format!(
        "Profile name to use: {name}\n\
         Sniffed encoding (override if wrong): {encoding}\n\n\
         Below is the head of the CSV/TSV file. Generate the TOML profile.\n\n\
         ----- BEGIN FILE HEAD -----\n\
         {head}\n\
         ----- END FILE HEAD -----\n"
    )
}

/// Decode at most `HEAD_BYTES` of the input as either UTF-8 (preferred) or
/// Shift_JIS (Japanese banks ship this), then trim to `HEAD_LINES`. Returns
/// the decoded head and the encoding label we picked.
pub(super) fn sniff_head(bytes: &[u8]) -> (String, &'static str) {
    let head_bytes = &bytes[..bytes.len().min(HEAD_BYTES)];
    let (text, encoding) = match std::str::from_utf8(head_bytes) {
        Ok(s) => (s.to_string(), "utf-8"),
        Err(_) => {
            // The slice may have ended mid-codepoint, so try a lossy UTF-8
            // decode first; only fall through to Shift_JIS if that produces
            // replacement characters.
            let (cow, _, had_errors) = encoding_rs::UTF_8.decode(head_bytes);
            if !had_errors {
                (cow.into_owned(), "utf-8")
            } else {
                let (cow, _, _) = encoding_rs::SHIFT_JIS.decode(head_bytes);
                (cow.into_owned(), "shift_jis")
            }
        }
    };
    let trimmed = take_lines(&text, HEAD_LINES);
    (trimmed, encoding)
}

fn take_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Strip a fenced code block if the model wrapped its TOML in one.
/// Tolerates ` ```toml `, ` ``` `, leading/trailing whitespace.
pub(super) fn strip_code_fence(s: &str) -> &str {
    let trimmed = s.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_lang = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    let body = after_lang.trim_end();
    body.strip_suffix("```").map(str::trim_end).unwrap_or(body)
}

/// Derive a profile name from `--name` or fall back to the input file stem.
/// Sanitises so the result is safe to use as a filename component.
pub(super) fn profile_name(explicit: Option<&str>, input: &str) -> anyhow::Result<String> {
    if let Some(n) = explicit {
        validate_name(n)?;
        return Ok(n.to_string());
    }
    let stem = Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("cannot derive a profile name from input {input:?}; pass --name")
        })?;
    let sanitised: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if sanitised.chars().all(|c| c == '_') {
        anyhow::bail!(
            "cannot derive a usable profile name from input {input:?}; pass --name <name>"
        );
    }
    Ok(sanitised)
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("--name must not be empty");
    }
    if name
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        anyhow::bail!("--name {name:?}: only letters, digits, '-', and '_' allowed");
    }
    Ok(())
}

fn save_path_for(name: &str) -> anyhow::Result<PathBuf> {
    Ok(profile_dir()?.join(format!("{name}.toml")))
}

/// `~/.config/fetchdoc/profiles/` (mirrors `Profile::resolve`'s lookup).
fn profile_dir() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("fetchdoc/profiles"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME not set; cannot pick a save location"))?;
    Ok(PathBuf::from(home).join(".config/fetchdoc/profiles"))
}

fn write_profile(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fence_with_lang() {
        let s = "```toml\nname = \"x\"\n```";
        assert_eq!(strip_code_fence(s), "name = \"x\"");
    }

    #[test]
    fn strip_fence_without_lang() {
        let s = "```\nname = \"x\"\n```";
        assert_eq!(strip_code_fence(s), "name = \"x\"");
    }

    #[test]
    fn strip_fence_passthrough_when_unfenced() {
        let s = "name = \"x\"\nencoding = \"utf-8\"";
        assert_eq!(strip_code_fence(s), s);
    }

    #[test]
    fn strip_fence_tolerates_outer_whitespace() {
        let s = "\n\n```toml\nname = \"x\"\n```\n\n";
        assert_eq!(strip_code_fence(s), "name = \"x\"");
    }

    #[test]
    fn sniff_utf8() {
        let (text, enc) = sniff_head("hello\nworld\n".as_bytes());
        assert_eq!(enc, "utf-8");
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn sniff_shift_jis() {
        let (encoded, _, _) = encoding_rs::SHIFT_JIS.encode("年月日,内容\n2026/04/30,テスト\n");
        let (text, enc) = sniff_head(&encoded);
        assert_eq!(enc, "shift_jis");
        assert!(text.contains("年月日"));
    }

    #[test]
    fn sniff_caps_at_head_lines() {
        let mut input = String::new();
        for i in 0..200 {
            input.push_str(&format!("row{i}\n"));
        }
        let (text, _) = sniff_head(input.as_bytes());
        assert_eq!(text.lines().count(), HEAD_LINES);
    }

    #[test]
    fn profile_name_explicit() {
        assert_eq!(
            profile_name(Some("smbc"), "/tmp/whatever.csv").unwrap(),
            "smbc"
        );
    }

    #[test]
    fn profile_name_from_stem_lowercases_and_keeps_hyphens() {
        assert_eq!(
            profile_name(None, "/tmp/Statement-2026.csv").unwrap(),
            "statement-2026"
        );
    }

    #[test]
    fn profile_name_rejects_pure_unicode_stem() {
        // Every non-ASCII char becomes `_`; a stem of all-`_` is useless,
        // so we refuse rather than silently producing `__.toml`.
        let err = profile_name(None, "/tmp/明細.csv").unwrap_err();
        assert!(err.to_string().contains("--name"));
    }

    #[test]
    fn profile_name_keeps_ascii_when_mixed_with_unicode() {
        // "smbc明細" → "smbc__" — non-ASCII chars become `_`, ASCII passes through.
        assert_eq!(profile_name(None, "/tmp/smbc明細.csv").unwrap(), "smbc__");
    }

    #[test]
    fn profile_name_rejects_invalid_explicit() {
        assert!(profile_name(Some("bad name!"), "x.csv").is_err());
        assert!(profile_name(Some(""), "x.csv").is_err());
    }

    #[test]
    fn user_prompt_includes_name_and_head() {
        let p = build_user_prompt("smbc", "shift_jis", "年月日,内容\n2026/04/30,X");
        assert!(p.contains("Profile name to use: smbc"));
        assert!(p.contains("shift_jis"));
        assert!(p.contains("年月日"));
        assert!(p.contains("BEGIN FILE HEAD"));
    }
}
