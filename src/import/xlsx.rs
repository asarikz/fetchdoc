//! `import xlsx` — parse an xlsx workbook into Transaction JSONL.
//!
//! Same profile-driven model as `import csv`: a TOML profile says which
//! sheet column maps to what. Reuses [`crate::import::csv::emit_records`]
//! after stringifying calamine cells into [`csv::StringRecord`]s, so the
//! per-row business logic (signed-amount math, date parsing, dedup id)
//! stays in one place.

use crate::import::Profile;
use crate::import::csv::{build_index, emit_records};
use anyhow::Context;
use calamine::{Data, Reader, Xlsx, open_workbook};
use clap::Args;

#[derive(Args, Debug)]
pub struct XlsxArgs {
    /// Path to the .xlsx file.
    pub input: String,

    /// Profile name (in `~/.config/fetchdoc/profiles/`) or path to a `.toml`.
    #[arg(long, conflicts_with = "infer")]
    pub profile: Option<String>,

    /// Hand the first ~50 rows of the chosen sheet to Anthropic and generate
    /// a profile TOML. The result is saved to
    /// `~/.config/fetchdoc/profiles/<name>.toml` (see `--name`) and used to
    /// parse the workbook. Per-row data is never sent on subsequent runs.
    #[arg(long, default_value_t = false)]
    pub infer: bool,

    /// Profile name to save under when using `--infer`. Defaults to the input
    /// file's stem. Ignored without `--infer`.
    #[arg(long, requires = "infer")]
    pub name: Option<String>,

    /// Sheet to import: either a name (e.g. `"明細"`) or a 0-indexed number
    /// as a string (e.g. `"0"`). Defaults to the first sheet.
    #[arg(long)]
    pub sheet: Option<String>,

    /// Suppress per-row stderr progress.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

pub async fn run(args: XlsxArgs) -> anyhow::Result<()> {
    if args.infer {
        return super::infer::run_xlsx(&args).await;
    }
    let profile_value = args
        .profile
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--profile is required (or use --infer)"))?;
    let profile = Profile::resolve(profile_value)
        .with_context(|| format!("loading profile {profile_value}"))?;

    run_with_profile(&args.input, args.sheet.as_deref(), &profile, args.quiet)
}

/// Open the workbook, pick a sheet, and emit Transaction JSONL using `profile`.
/// Shared by the deterministic path and `--infer`.
pub(super) fn run_with_profile(
    input: &str,
    sheet: Option<&str>,
    profile: &Profile,
    quiet: bool,
) -> anyhow::Result<()> {
    let mut wb: Xlsx<_> = open_workbook(input).with_context(|| format!("opening {input}"))?;
    let range = pick_sheet(&mut wb, sheet)?;

    // Convert all rows up-front — xlsx files are small, and calamine's
    // `Range::rows()` iterator borrows the workbook so it's awkward to
    // thread through `emit_records` without buffering anyway.
    let all_rows: Vec<csv::StringRecord> = range
        .rows()
        .map(|cells| {
            let strs: Vec<String> = cells.iter().map(stringify_cell).collect();
            csv::StringRecord::from(strs)
        })
        .collect();

    let header_idx = profile.header_row.saturating_sub(1);
    let headers = all_rows
        .get(header_idx)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "header_row = {} but sheet only has {} rows",
                profile.header_row,
                all_rows.len()
            )
        })?
        .clone();
    let idx = build_index(&headers, &profile.columns)
        .with_context(|| format!("profile {} header lookup", profile.name))?;

    let data_start = header_idx + 1 + profile.skip_rows;
    let records = all_rows
        .into_iter()
        .skip(data_start)
        .filter(|r| !is_blank_row(r))
        .map(Ok::<_, anyhow::Error>);

    emit_records(records, &idx, profile, "xlsx", input, quiet)
}

/// Stringify the first `max_rows` rows of the chosen sheet as comma-separated
/// text so [`super::infer`] can show them to Anthropic with the same prompt
/// the CSV path uses. Cells are escaped with the standard CSV rules so
/// embedded commas / quotes / newlines don't fool the model.
pub(super) fn head_as_csv(
    input: &str,
    sheet: Option<&str>,
    max_rows: usize,
) -> anyhow::Result<String> {
    let mut wb: Xlsx<_> = open_workbook(input).with_context(|| format!("opening {input}"))?;
    let range = pick_sheet(&mut wb, sheet)?;
    let mut out = String::new();
    for row in range.rows().take(max_rows) {
        let cells: Vec<String> = row.iter().map(stringify_cell).map(csv_escape).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    Ok(out)
}

/// Quote a single CSV field per RFC 4180 (only when needed).
fn csv_escape(s: String) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s
    }
}

/// Select sheet by name or 0-indexed number-as-string. Defaults to first sheet.
fn pick_sheet<R: std::io::Read + std::io::Seek>(
    wb: &mut Xlsx<R>,
    selector: Option<&str>,
) -> anyhow::Result<calamine::Range<Data>> {
    let names = wb.sheet_names();
    if names.is_empty() {
        anyhow::bail!("workbook has no sheets");
    }
    let target = match selector {
        None => names[0].clone(),
        Some(s) => {
            if let Ok(i) = s.parse::<usize>() {
                names
                    .get(i)
                    .ok_or_else(|| {
                        anyhow::anyhow!("sheet index {i} out of range (have {})", names.len())
                    })?
                    .clone()
            } else {
                names
                    .iter()
                    .find(|n| n.as_str() == s)
                    .ok_or_else(|| anyhow::anyhow!("sheet {s:?} not found in {names:?}"))?
                    .clone()
            }
        }
    };
    wb.worksheet_range(&target)
        .with_context(|| format!("reading sheet {target:?}"))
}

/// Stringify one cell so the rest of the pipeline can treat xlsx and csv
/// uniformly. Excel's serial-number dates are converted to `YYYY-MM-DD`;
/// numeric cells with no fractional part lose the `.0`.
fn stringify_cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Bool(b) => b.to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#{e:?}"),
    }
}

fn is_blank_row(r: &csv::StringRecord) -> bool {
    r.iter().all(|s| s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stringify_int_float_date() {
        assert_eq!(stringify_cell(&Data::Empty), "");
        assert_eq!(stringify_cell(&Data::Int(12100)), "12100");
        assert_eq!(stringify_cell(&Data::Float(12100.0)), "12100");
        assert_eq!(stringify_cell(&Data::Float(12.5)), "12.5");
        assert_eq!(stringify_cell(&Data::String("Acme".into())), "Acme");
        assert_eq!(stringify_cell(&Data::Bool(true)), "true");
    }

    #[test]
    fn csv_escape_quotes_when_needed() {
        assert_eq!(csv_escape("plain".into()), "plain");
        assert_eq!(csv_escape("with,comma".into()), "\"with,comma\"");
        assert_eq!(csv_escape("with\"quote".into()), "\"with\"\"quote\"");
        assert_eq!(csv_escape("with\nnewline".into()), "\"with\nnewline\"");
    }

    #[test]
    fn blank_row_detection() {
        let r = csv::StringRecord::from(vec!["", " ", "\t"]);
        assert!(is_blank_row(&r));
        let r = csv::StringRecord::from(vec!["", "x"]);
        assert!(!is_blank_row(&r));
    }
}
