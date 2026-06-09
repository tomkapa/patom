//! HMAC-SHA256 verification of inbound Lemon Squeezy webhooks (critical-tier
//! per CLAUDE.md §3 — 100% coverage target).
//!
//! Lemon Squeezy signs every webhook with HMAC-SHA256 over the **raw request
//! body**, key = the webhook signing secret, and sends the lowercase hex digest
//! in the `X-Signature` header (no timestamp, unlike Slack). We recompute the
//! MAC and constant-time compare; a branchless compare prevents a timing-channel
//! oracle. Pure function — no I/O.

use hmac::{Hmac, Mac};
use patom::types::SecretString;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::error::LemonSqueezyError;

type HmacSha256 = Hmac<Sha256>;

/// Verify a Lemon Squeezy webhook signature.
///
/// `raw_body` must be the exact bytes Lemon Squeezy delivered — any middleware
/// that re-serializes the body before verification produces a different MAC and
/// fails every signature. The handler takes `Bytes`, not `Json<T>`.
///
/// # Errors
/// [`LemonSqueezyError::SignatureMismatch`] if the recomputed HMAC does not
/// equal the header value (including a missing/oversized/garbage header).
pub fn verify_signature(
    secret: &SecretString,
    signature_header: &str,
    raw_body: &[u8],
) -> Result<(), LemonSqueezyError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| LemonSqueezyError::SignatureMismatch)?;
    mac.update(raw_body);
    let computed = mac.finalize().into_bytes();
    let computed_hex = hex_encode(&computed);

    // Constant-time over the hex encodings. `ct_eq` on unequal lengths returns
    // false without leaking where they diverge, so a wrong-length header is a
    // plain mismatch.
    if computed_hex
        .as_bytes()
        .ct_eq(signature_header.as_bytes())
        .into()
    {
        Ok(())
    } else {
        Err(LemonSqueezyError::SignatureMismatch)
    }
}

/// Lowercase hex of a byte slice. Lemon Squeezy emits lowercase, so a direct
/// string compare against the header works; also used to render the body
/// digest that keys webhook idempotency.
pub(super) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("invariant: write to String is infallible");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> SecretString {
        SecretString::try_from("ls_webhook_secret_value".to_string()).expect("secret")
    }

    /// Canonical signature for `(secret, body)`.
    fn sign(secret: &SecretString, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes()).expect("key");
        mac.update(body);
        hex_encode(&mac.finalize().into_bytes())
    }

    #[test]
    fn accepts_a_valid_signature() {
        let s = secret();
        let body = br#"{"meta":{"event_name":"subscription_created"}}"#;
        let sig = sign(&s, body);
        assert!(verify_signature(&s, &sig, body).is_ok());
    }

    #[test]
    fn rejects_a_tampered_body() {
        let s = secret();
        let sig = sign(&s, b"original");
        assert!(matches!(
            verify_signature(&s, &sig, b"tampered"),
            Err(LemonSqueezyError::SignatureMismatch),
        ));
    }

    #[test]
    fn rejects_a_signature_from_a_different_secret() {
        let s = secret();
        let other = SecretString::try_from("other_secret".to_string()).expect("secret");
        let body = b"payload";
        let sig = sign(&other, body);
        assert!(matches!(
            verify_signature(&s, &sig, body),
            Err(LemonSqueezyError::SignatureMismatch),
        ));
    }

    #[test]
    fn rejects_a_garbage_header() {
        let s = secret();
        assert!(matches!(
            verify_signature(&s, "not-hex", b"payload"),
            Err(LemonSqueezyError::SignatureMismatch),
        ));
    }

    #[test]
    fn rejects_an_empty_header() {
        let s = secret();
        assert!(matches!(
            verify_signature(&s, "", b"payload"),
            Err(LemonSqueezyError::SignatureMismatch),
        ));
    }
}
