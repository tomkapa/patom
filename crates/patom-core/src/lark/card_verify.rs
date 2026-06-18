//! Verification of an inbound Lark `card.action.trigger` callback (issue #214).
//!
//! Unlike the long-connection event stream (authenticated by the bot's
//! `tenant_access_token` on the WS dial), the card-callback arrives over a
//! public HTTPS route, so it must be verified per-request with the app's
//! console-configured secrets:
//!
//! 1. **Signature** — Lark signs the request with the **Encrypt Key**:
//!    `sha256(timestamp + nonce + encrypt_key + raw_body)`, hex, carried in the
//!    `X-Lark-Signature` header. Recompute over the *raw* bytes and compare
//!    constant-time (the route reads `Bytes`, never re-serialized JSON).
//! 2. **Token** — the body echoes the **Verification Token** (`header.token`);
//!    compare it constant-time too.
//!
//! Both comparisons use [`subtle`] to avoid a timing oracle. Replay is bounded
//! by the decision being idempotent (a re-played click re-decides the same row
//! with no effect — #213's double-click guard).
//!
//! NOTE (validate against the live console, per the issue): this implements the
//! documented *signed-plaintext* scheme. Body **encryption** (the `{"encrypt":…}`
//! envelope) is intentionally not supported in v1 — configure the app's card
//! request URL without encryption; the Encrypt Key still signs the request.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Recompute the Lark request signature and constant-time compare it to the
/// `X-Lark-Signature` header.
///
/// Returns `true` iff they match; a malformed (non-64-char) signature fails
/// before the compare.
#[must_use]
pub fn verify_signature(
    encrypt_key: &str,
    timestamp: &str,
    nonce: &str,
    raw_body: &[u8],
    provided_signature: &str,
) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(encrypt_key.as_bytes());
    hasher.update(raw_body);
    let digest = hasher.finalize();
    let expected = crate::hex::encode_32(&digest);
    // Lark sends lowercase hex; normalize the presented value so a case-only
    // difference is not treated as a forgery.
    constant_time_str_eq(
        expected.as_bytes(),
        provided_signature.to_ascii_lowercase().as_bytes(),
    )
}

/// Constant-time check that the body-echoed token equals the app's Verification
/// Token.
#[must_use]
pub fn verify_token(verification_token: &str, presented: &str) -> bool {
    constant_time_str_eq(verification_token.as_bytes(), presented.as_bytes())
}

/// Constant-time byte-equality. Length is compared first (a length difference is
/// not secret for fixed-width signatures/tokens); equal-length inputs go through
/// `subtle`'s branchless compare.
fn constant_time_str_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "test-encrypt-key";
    const TS: &str = "1700000000";
    const NONCE: &str = "abc123";

    fn sign(body: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(TS.as_bytes());
        hasher.update(NONCE.as_bytes());
        hasher.update(KEY.as_bytes());
        hasher.update(body);
        crate::hex::encode_32(&hasher.finalize())
    }

    #[test]
    fn accepts_a_correct_signature() {
        let body = br#"{"schema":"2.0"}"#;
        assert!(verify_signature(KEY, TS, NONCE, body, &sign(body)));
    }

    #[test]
    fn accepts_uppercase_hex_signature() {
        let body = br#"{"schema":"2.0"}"#;
        assert!(verify_signature(
            KEY,
            TS,
            NONCE,
            body,
            &sign(body).to_ascii_uppercase()
        ));
    }

    #[test]
    fn rejects_a_tampered_body() {
        let sig = sign(br#"{"schema":"2.0"}"#);
        assert!(!verify_signature(
            KEY,
            TS,
            NONCE,
            br#"{"schema":"evil"}"#,
            &sig
        ));
    }

    #[test]
    fn rejects_a_wrong_encrypt_key() {
        let body = b"hello";
        assert!(!verify_signature("other-key", TS, NONCE, body, &sign(body)));
    }

    #[test]
    fn rejects_a_malformed_signature() {
        assert!(!verify_signature(KEY, TS, NONCE, b"x", "not-hex"));
        assert!(!verify_signature(KEY, TS, NONCE, b"x", ""));
    }

    #[test]
    fn token_compare_is_exact() {
        assert!(verify_token("vtok-123", "vtok-123"));
        assert!(!verify_token("vtok-123", "vtok-124"));
        assert!(!verify_token("vtok-123", "vtok-123-extra"));
        assert!(!verify_token("vtok-123", ""));
    }
}
