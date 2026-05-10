//! Google OAuth specifics: client_secret loading, authorisation URL,
//! token exchange.

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

/// OAuth scope: read-only access to the user's Gmail.
pub const SCOPE_GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// OAuth scope: per-file access to Drive files created by this app.
#[allow(dead_code)] // Wired up by `export drive` (planned for v0.3).
pub const SCOPE_DRIVE_FILE: &str = "https://www.googleapis.com/auth/drive.file";

/// Authorisation endpoint.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Token exchange endpoint.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The shape of the `client_secret.json` Google Cloud Console produces for a
/// **Desktop application** OAuth client. Web-application clients use the
/// `web` key instead — we reject those because they require a pre-registered
/// redirect URI which BYO-credentials users cannot provide.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientSecret {
    pub client_id: String,
    pub client_secret: String,
    /// Auth endpoint as written in the JSON. Falls back to [`AUTH_URL`].
    #[serde(default)]
    pub auth_uri: Option<String>,
    /// Token endpoint as written in the JSON. Falls back to [`TOKEN_URL`].
    #[serde(default)]
    pub token_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClientSecretFile {
    installed: Option<ClientSecret>,
    web: Option<serde_json::Value>,
}

impl ClientSecret {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading client_secret from {}", path.display()))?;
        let parsed: ClientSecretFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing client_secret JSON from {}", path.display()))?;
        if parsed.installed.is_none() && parsed.web.is_some() {
            anyhow::bail!(
                "{} is a Web-application OAuth client; fetchdoc requires a Desktop \
                 application client (the JSON has an `installed` key, not `web`). \
                 Re-create the OAuth client in GCP Console with type 'Desktop app'.",
                path.display()
            );
        }
        parsed.installed.ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no `installed` block; not a valid Desktop OAuth client_secret",
                path.display()
            )
        })
    }

    pub fn auth_url(&self) -> &str {
        self.auth_uri.as_deref().unwrap_or(AUTH_URL)
    }

    pub fn token_url(&self) -> &str {
        self.token_uri.as_deref().unwrap_or(TOKEN_URL)
    }
}

/// Build the authorisation URL the user opens in their browser.
///
/// `access_type=offline` + `prompt=consent` is what makes Google return a
/// refresh token even on subsequent logins; without `prompt=consent` Google
/// will skip the consent screen and omit the refresh token if one was
/// already issued.
pub fn build_auth_url(
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    code_challenge: &str,
    state: &str,
    auth_endpoint: &str,
) -> String {
    let mut url = url::Url::parse(auth_endpoint).expect("auth endpoint is a valid URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url.into()
}

/// Successful response from the token exchange endpoint. We only keep the
/// fields fetchdoc actually uses; Google sends a few more (`id_token`,
/// `expires_in`, `token_type`, `scope`) we deliberately ignore.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    /// Short-lived bearer token. `auth login` discards it (only the
    /// refresh_token is persisted); `fetch gmail` re-acquires one per run via
    /// [`refresh_access_token`].
    pub access_token: String,
    /// Present on the **first** consent (or every consent if
    /// `prompt=consent` was passed). Subsequent silent re-auths omit it.
    pub refresh_token: Option<String>,
}

/// Exchange the authorisation code for tokens using the PKCE verifier.
pub async fn exchange_code(
    secret: &ClientSecret,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    http: &reqwest::Client,
) -> anyhow::Result<TokenResponse> {
    let resp = http
        .post(secret.token_url())
        .form(&[
            ("client_id", secret.client_id.as_str()),
            ("client_secret", secret.client_secret.as_str()),
            ("code", code),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("posting to token endpoint")?;
    let status = resp.status();
    let body = resp.text().await.context("reading token endpoint body")?;
    if !status.is_success() {
        anyhow::bail!("token exchange failed ({status}): {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing token response: {body}"))
}

/// Use a stored refresh_token to obtain a fresh short-lived access token. A
/// 401/`invalid_grant` here usually means the user revoked access; the caller
/// should ask them to re-run `fetchdoc auth login`.
pub async fn refresh_access_token(
    secret: &ClientSecret,
    refresh_token: &str,
    http: &reqwest::Client,
) -> anyhow::Result<String> {
    let resp = http
        .post(secret.token_url())
        .form(&[
            ("client_id", secret.client_id.as_str()),
            ("client_secret", secret.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("posting to token endpoint")?;
    let status = resp.status();
    let body = resp.text().await.context("reading token endpoint body")?;
    if !status.is_success() {
        anyhow::bail!(
            "refresh_token exchange failed ({status}): {body}. \
             If this persists, re-run `fetchdoc auth login --source gmail`."
        );
    }
    let token: TokenResponse =
        serde_json::from_str(&body).with_context(|| format!("parsing token response: {body}"))?;
    Ok(token.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_pkce_and_offline() {
        let url = build_auth_url(
            "id123",
            "http://127.0.0.1:1234",
            SCOPE_GMAIL_READONLY,
            "challenge",
            "state",
            AUTH_URL,
        );
        assert!(url.contains("client_id=id123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("scope=https"));
        assert!(url.contains("state=state"));
    }

    #[test]
    fn loads_desktop_client_secret() {
        let tmp = std::env::temp_dir().join(format!("fetchdoc-cs-{}.json", std::process::id()));
        std::fs::write(
            &tmp,
            r#"{"installed":{"client_id":"abc","client_secret":"shh"}}"#,
        )
        .unwrap();
        let cs = ClientSecret::load(&tmp).unwrap();
        assert_eq!(cs.client_id, "abc");
        assert_eq!(cs.client_secret, "shh");
        assert_eq!(cs.auth_url(), AUTH_URL);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rejects_web_client_secret_with_explanation() {
        let tmp = std::env::temp_dir().join(format!("fetchdoc-cs-web-{}.json", std::process::id()));
        std::fs::write(&tmp, r#"{"web":{"client_id":"abc","client_secret":"shh"}}"#).unwrap();
        let err = ClientSecret::load(&tmp).unwrap_err().to_string();
        assert!(err.contains("Desktop"), "got: {err}");
        let _ = std::fs::remove_file(&tmp);
    }
}
