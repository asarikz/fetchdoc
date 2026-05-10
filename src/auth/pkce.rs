//! PKCE (Proof Key for Code Exchange, RFC 7636) helpers for the OAuth flow.
//!
//! The CLI binds to a random port on `127.0.0.1`, opens the system browser to
//! the Google authorisation endpoint with `code_challenge`, then receives the
//! authorisation code on its loopback redirect handler and exchanges it for
//! a refresh token using `code_verifier`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A freshly generated PKCE pair.
#[derive(Debug)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a new `(verifier, challenge)` pair using SHA-256 (`S256` method).
///
/// RFC 7636 §4.1: verifier is 43..=128 unreserved characters. We use 32 bytes
/// of OS randomness base64url-encoded → exactly 43 characters, the minimum
/// the spec recommends and what every reference implementation uses.
pub fn generate() -> PkcePair {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    PkcePair {
        verifier,
        challenge,
    }
}

/// Generate a random `state` parameter for CSRF protection on the OAuth
/// callback. 16 bytes of OS randomness → 22 base64url chars.
pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_meets_rfc_7636_format() {
        let p = generate();
        assert_eq!(p.verifier.len(), 43, "32-byte b64url should be 43 chars");
        // unreserved: [A-Z][a-z][0-9]-._~ — base64url uses [A-Z][a-z][0-9]-_
        for c in p.verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-PKCE char {c:?}"
            );
        }
    }

    #[test]
    fn challenge_is_sha256_of_verifier() {
        let p = generate();
        let recomputed = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, recomputed);
    }

    #[test]
    fn pairs_are_unique() {
        let a = generate();
        let b = generate();
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn state_is_url_safe() {
        let s = random_state();
        for c in s.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-url-safe char {c:?}"
            );
        }
    }
}
