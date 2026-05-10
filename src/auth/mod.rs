//! Authentication subcommand: OAuth flows and token storage.
//!
//! `fetchdoc` follows the BYO-credentials model: the user creates their own
//! OAuth client in their own Google Cloud project (Desktop application type)
//! and points `fetchdoc` at the resulting `client_secret.json`. This avoids
//! the need for centralised app verification (CASA Tier 2 for `gmail.readonly`).
//!
//! Refresh tokens are stored in the OS keychain via the `keyring` crate
//! (macOS Keychain, Windows Credential Manager, Linux Secret Service).

use clap::{Args, Subcommand, ValueEnum};

mod google;
mod pkce;
mod storage;

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// Set up OAuth client credentials (BYO from your own Google Cloud project).
    ///
    /// Walks you through enabling Gmail/Drive APIs, creating an OAuth Desktop
    /// client, and saving the resulting `client_secret.json` to a known path.
    Init,
    /// Open browser, complete OAuth flow, store the refresh token in your OS keychain.
    Login {
        /// Which upstream service to authenticate.
        #[arg(long, value_enum)]
        source: SourceKind,
    },
    /// Show currently authenticated identities and token status.
    Status,
    /// Remove a stored credential.
    Logout {
        #[arg(long, value_enum)]
        source: SourceKind,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum SourceKind {
    Gmail,
}

pub async fn run(args: AuthArgs) -> anyhow::Result<()> {
    match args.command {
        AuthCommand::Init => {
            anyhow::bail!("auth init: not implemented yet (see issue #46)")
        }
        AuthCommand::Login { source: _ } => {
            anyhow::bail!("auth login: not implemented yet (see issue #46)")
        }
        AuthCommand::Status => {
            anyhow::bail!("auth status: not implemented yet (see issue #46)")
        }
        AuthCommand::Logout { source: _ } => {
            anyhow::bail!("auth logout: not implemented yet (see issue #46)")
        }
    }
}
