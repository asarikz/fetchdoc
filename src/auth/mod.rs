//! Authentication subcommand: OAuth flows and token storage.
//!
//! `fetchdoc` follows the BYO-credentials model: the user creates their own
//! OAuth client in their own Google Cloud project (Desktop application type)
//! and points `fetchdoc` at the resulting `client_secret.json`. This avoids
//! the need for centralised app verification (CASA Tier 2 for `gmail.readonly`).
//!
//! Refresh tokens are stored in the OS keychain via the `keyring` crate
//! (macOS Keychain, Windows Credential Manager, Linux Secret Service).

use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

mod google;
mod loopback;
mod paths;
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
    /// Prints the GCP Console steps. With `--from <PATH>`, copies the
    /// downloaded `client_secret.json` to `~/.config/fetchdoc/`.
    Init {
        /// Path to a `client_secret.json` you downloaded from GCP Console.
        /// If set, the file is copied to the canonical config location.
        #[arg(long)]
        from: Option<PathBuf>,
    },
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

impl SourceKind {
    fn keychain_account(self) -> &'static str {
        match self {
            SourceKind::Gmail => "gmail:default",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SourceKind::Gmail => "Gmail",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            SourceKind::Gmail => google::SCOPE_GMAIL_READONLY,
        }
    }
}

pub async fn run(args: AuthArgs) -> anyhow::Result<()> {
    match args.command {
        AuthCommand::Init { from } => init(from),
        AuthCommand::Login { source } => login(source).await,
        AuthCommand::Status => status(),
        AuthCommand::Logout { source } => logout(source),
    }
}

fn init(from: Option<PathBuf>) -> anyhow::Result<()> {
    let dest = paths::client_secret_path()?;
    let dest_dir = dest
        .parent()
        .expect("client_secret_path has a parent")
        .to_path_buf();

    if let Some(src) = from {
        std::fs::create_dir_all(&dest_dir)
            .with_context(|| format!("creating {}", dest_dir.display()))?;
        // Validate before saving so we never write a non-Desktop client.
        let _ = google::ClientSecret::load(&src)?;
        std::fs::copy(&src, &dest)
            .with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
        eprintln!("auth init: saved client_secret to {}", dest.display());
        return Ok(());
    }

    eprintln!(
        "fetchdoc uses your own OAuth client (BYO-credentials). One-time setup:

  1. Go to https://console.cloud.google.com/ and create (or pick) a project.
  2. APIs & Services → Library → enable 'Gmail API'.
  3. APIs & Services → OAuth consent screen → External, fill in the basics,
     add yourself as a Test user, and select the scope:
       https://www.googleapis.com/auth/gmail.readonly
  4. APIs & Services → Credentials → Create Credentials → OAuth client ID.
     Application type: 'Desktop app'. Download the JSON.
  5. Run:
       fetchdoc auth init --from /path/to/client_secret_xxxx.json

The file will be copied to {}.

Then:
       fetchdoc auth login --source gmail",
        dest.display()
    );
    Ok(())
}

async fn login(source: SourceKind) -> anyhow::Result<()> {
    let secret_path = paths::client_secret_path()?;
    let secret = google::ClientSecret::load(&secret_path).with_context(|| {
        format!(
            "loading {}. Run `fetchdoc auth init` first.",
            secret_path.display()
        )
    })?;

    let pkce = pkce::generate();
    let state = pkce::random_state();
    let (listener, redirect_uri) = loopback::bind().await?;

    let auth_url = google::build_auth_url(
        &secret.client_id,
        &redirect_uri,
        source.scope(),
        &pkce.challenge,
        &state,
        secret.auth_url(),
    );

    eprintln!(
        "Opening browser for {} authorisation. If it does not open automatically, paste this URL:\n\n  {auth_url}\n",
        source.label()
    );
    // `open` system call via `webbrowser` would be nicer but we keep deps
    // minimal — most users see the URL in stderr and click it themselves.
    if let Err(e) = open_in_browser(&auth_url) {
        eprintln!("(could not auto-open browser: {e})");
    }

    let success_msg = format!("Logged in to {} via fetchdoc", source.label());
    let params = loopback::accept_one(listener, &success_msg).await?;

    if let Some(err) = params.error.as_deref() {
        anyhow::bail!("OAuth provider returned error: {err}");
    }
    let code = params
        .code
        .ok_or_else(|| anyhow::anyhow!("OAuth callback missing `code` parameter"))?;
    let returned_state = params.state.unwrap_or_default();
    if returned_state != state {
        anyhow::bail!(
            "OAuth state mismatch (expected {state}, got {returned_state}); \
             possible CSRF — aborting"
        );
    }

    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;
    let token = google::exchange_code(&secret, &code, &pkce.verifier, &redirect_uri, &http)
        .await
        .context("exchanging authorisation code")?;
    let refresh = token.refresh_token.ok_or_else(|| {
        anyhow::anyhow!(
            "Google returned no refresh_token; remove fetchdoc from your account's \
             authorised apps at https://myaccount.google.com/permissions and retry"
        )
    })?;

    storage::store(source.keychain_account(), &refresh)
        .with_context(|| format!("saving refresh token for {}", source.label()))?;
    eprintln!(
        "auth login: stored refresh token for {} in OS keychain",
        source.label()
    );
    Ok(())
}

fn status() -> anyhow::Result<()> {
    let secret_path = paths::client_secret_path()?;
    let secret_present = secret_path.exists();
    eprintln!(
        "client_secret: {} ({})",
        secret_path.display(),
        if secret_present {
            "present"
        } else {
            "missing — run `fetchdoc auth init`"
        }
    );

    // Single source today; the loop pattern lets us add Outlook etc. without
    // restructuring the output.
    let sources = [SourceKind::Gmail];
    for source in sources {
        let token = storage::load(source.keychain_account())?;
        let state = if token.is_some() {
            "authenticated"
        } else {
            "not authenticated"
        };
        eprintln!("  {}: {state}", source.label());
    }
    Ok(())
}

fn logout(source: SourceKind) -> anyhow::Result<()> {
    storage::delete(source.keychain_account())?;
    eprintln!("auth logout: removed credential for {}", source.label());
    Ok(())
}

/// Best-effort browser launch. Pure-std rather than pulling `webbrowser`.
fn open_in_browser(url: &str) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {cmd}"))?;
    Ok(())
}
