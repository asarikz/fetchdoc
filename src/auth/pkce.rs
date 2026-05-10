//! PKCE (Proof Key for Code Exchange, RFC 7636) helpers for the OAuth flow.
//!
//! The CLI binds to a random port on `127.0.0.1`, opens the system browser to
//! the Google authorisation endpoint with `code_challenge`, then receives the
//! authorisation code on its loopback redirect handler and exchanges it for
//! a refresh token using `code_verifier`.

#![allow(dead_code)] // Stub — wired up in the auth implementation issue.

/// A freshly generated PKCE pair.
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a new `(verifier, challenge)` pair using SHA-256 (`S256` method).
pub fn generate() -> PkcePair {
    // TODO: implement using `rand` + `sha2` + base64url. Stub for scaffold.
    unimplemented!("pkce::generate")
}
