//! Gmail source: list messages matching a query, fetch PDF attachments,
//! emit one JSONL record per attachment on stdout.

use clap::Args;

#[derive(Args, Debug)]
pub struct GmailArgs {
    /// Only fetch messages received on or after this date (YYYY-MM-DD).
    #[arg(long)]
    pub since: Option<String>,

    /// Gmail search query (e.g. `"has:attachment filename:pdf 請求書"`).
    #[arg(long, default_value = "has:attachment filename:pdf")]
    pub query: String,

    /// Stop after this many messages.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Directory to cache downloaded attachments. Defaults to a per-OS cache dir.
    #[arg(long)]
    pub cache_dir: Option<String>,
}

pub async fn run(_args: GmailArgs) -> anyhow::Result<()> {
    anyhow::bail!("fetch gmail: not implemented yet (see issue #10)")
}
