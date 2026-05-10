//! `import csv --infer` — generate a profile by handing the file head to
//! an LLM. Lands once the shared Anthropic client is in place; for now this
//! module just declares the contract so the CLI surface is stable.
//!
//! Design notes:
//! - We send the **first 50 rows** + the header (or first 4 KB, whichever
//!   smaller) — enough to disambiguate column meaning, small enough to be
//!   one cheap call.
//! - The model is asked to emit a TOML profile matching [`super::Profile`].
//! - We save it to `~/.config/fetchdoc/profiles/<inferred-name>.toml` and
//!   then run the deterministic parser. Per-row data NEVER goes to the API.

use super::csv::CsvArgs;

pub async fn run_csv(_args: &CsvArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "import csv --infer: not implemented yet. \
         Generate a profile by hand for now \
         (~/.config/fetchdoc/profiles/<name>.toml) and rerun with --profile <name>."
    )
}
