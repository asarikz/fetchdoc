//! GnuCash CSV export.
//!
//! Targets the GnuCash 4.x+ "Import Transactions from CSV" format. Each
//! invoice becomes one transaction with two splits: a debit to an expense
//! account and a credit to an accounts-payable (or bank) account.
//!
//! The CSV column order matches GnuCash's default importer settings so the
//! user only has to map the account columns once.

use clap::Args;

#[derive(Args, Debug)]
pub struct GnucashArgs {
    /// Output CSV path. Use `-` for stdout.
    #[arg(long, default_value = "-")]
    pub out: String,

    /// Default debit account (the expense bucket new transactions go into).
    /// Example: `Expenses:諸経費`.
    #[arg(long)]
    pub debit_account: String,

    /// Default credit account (where the money came from).
    /// Example: `Liabilities:買掛金` or `Assets:Current:三井住友`.
    #[arg(long)]
    pub credit_account: String,

    /// Currency commodity code. Defaults to JPY.
    #[arg(long, default_value = "JPY")]
    pub currency: String,
}

pub async fn run(_args: GnucashArgs) -> anyhow::Result<()> {
    anyhow::bail!("export gnucash: not implemented yet (★ priority — see issue #46)")
}
