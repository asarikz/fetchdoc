use clap::Parser;

mod anthropic;
mod auth;
mod classify;
mod cli;
mod export;
mod import;
mod invoicing_jp;
mod io;
mod sources;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    cli.run().await
}
