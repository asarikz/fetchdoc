//! `fetch <source>` subcommand: pull documents from upstream services.
//!
//! Each source emits one JSON Lines record per document on stdout. The
//! attachment itself is written to a local cache directory and referenced
//! by `attachment_path` in the record.

use clap::{Args, Subcommand};

mod dir;
mod eml;
mod gmail;
mod maildir;
mod mbox;

// Re-exported so the `render-body` command (and any future consumer outside
// this module) can reuse `pick_primary_body_part` / `find_part_by_index` /
// `default_cache_dir` without having to live under `fetch`.
pub mod mail;

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[command(subcommand)]
    command: FetchCommand,
}

#[derive(Subcommand, Debug)]
enum FetchCommand {
    /// Fetch invoice attachments from Gmail.
    Gmail(gmail::GmailArgs),
    /// Pull PDF attachments out of locally-stored `.eml` files.
    Eml(eml::EmlArgs),
    /// Pull PDF attachments out of locally-stored mbox archives.
    Mbox(mbox::MboxArgs),
    /// Pull PDF attachments out of locally-stored Maildir trees.
    Maildir(maildir::MaildirArgs),
    /// Ingest pre-downloaded PDFs from a local folder (Amazon / ヨドバシ /
    /// NUROモバイル の領収書 etc. that the user saved by hand).
    Dir(dir::DirArgs),
}

pub async fn run(args: FetchArgs) -> anyhow::Result<()> {
    match args.command {
        FetchCommand::Gmail(a) => gmail::run(a).await,
        FetchCommand::Eml(a) => eml::run(a).await,
        FetchCommand::Mbox(a) => mbox::run(a).await,
        FetchCommand::Maildir(a) => maildir::run(a).await,
        FetchCommand::Dir(a) => dir::run(a).await,
    }
}
