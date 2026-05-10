//! Google OAuth specifics: authorisation URL, scopes, token exchange endpoint.

#![allow(dead_code)] // Stub — wired up in the auth implementation issue.

/// OAuth scope: read-only access to the user's Gmail.
pub const SCOPE_GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// OAuth scope: per-file access to Drive files created by this app.
pub const SCOPE_DRIVE_FILE: &str = "https://www.googleapis.com/auth/drive.file";

/// Authorisation endpoint.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Token exchange endpoint.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
