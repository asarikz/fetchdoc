//! Local filesystem export.
//!
//! Writes the cached PDF to a structured location with a filename template
//! that satisfies the Japanese e-bookkeeping (電帳法) search requirement of
//! "transaction date / total amount / counterparty name".
//!
//! Default template:
//!     `{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf`

use clap::Args;

#[derive(Args, Debug)]
pub struct LocalArgs {
    /// Root directory to write under.
    #[arg(long)]
    pub root: String,

    /// Filename template. See module docs for substitution variables.
    #[arg(
        long,
        default_value = "{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf"
    )]
    pub name_template: String,
}

pub async fn run(_args: LocalArgs) -> anyhow::Result<()> {
    anyhow::bail!("export local: not implemented yet (see issue #20)")
}
