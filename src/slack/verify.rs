//! HMAC-SHA256 verification of inbound Slack webhooks (critical-tier per
//! CLAUDE.md §3 — 100% coverage target).
//!
//! Slack signs every webhook with HMAC-SHA256 over
//! `v0:<timestamp>:<raw_body>`, key = the signing secret. The header carries
//! `X-Slack-Signature: v0=<hex64>` and `X-Slack-Request-Timestamp: <epoch>`.
//!
//! Two gates:
//! 1. **Freshness.** Reject if `|now - timestamp| > SLACK_TIMESTAMP_MAX_SKEW`
//!    (Slack's recommended 5 minutes). Without this gate, a captured
//!    payload + signature pair could be replayed indefinitely.
//! 2. **Equality.** Constant-time compare against the recomputed
//!    signature. Branchless compare prevents a timing-channel oracle.
//!
//! Pure function: `(secret, timestamp, signature, raw_body, now)` in,
//! `Result<(), VerifyError>` out. No I/O, no allocation beyond the
//! single buffer the MAC needs internally.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::types::SecretString;

use super::limits::SLACK_TIMESTAMP_MAX_SKEW;
use super::types::{SlackEventTimestamp, SlackSignature};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Timestamp is outside the freshness window. Most likely a replay
    /// of a captured payload, or — far less commonly — clock drift.
    #[error("timestamp out of range (skew exceeds {max_secs}s)")]
    StaleTimestamp { max_secs: u64 },

    /// Recomputed signature does not equal the provided header.
    #[error("signature mismatch")]
    Mismatch,

    /// Hex decoding of the signature tail failed (e.g. an odd number of
    /// chars slipped past the `SlackSignature::try_from` guard somehow).
    /// Defensive — should be impossible if the newtype is constructed
    /// correctly.
    #[error("malformed signature: {0}")]
    MalformedSignature(&'static str),
}

/// Verify a Slack webhook signature.
///
/// `raw_body` is the exact bytes Slack delivered — any middleware that
/// re-serializes the body before verification will produce a different
/// MAC and fail every signature. The handler must take `Bytes`, not
/// `Json<T>`.
///
/// # Errors
/// Returns `VerifyError::StaleTimestamp` if the request timestamp is
/// outside `±SLACK_TIMESTAMP_MAX_SKEW` of `now`. Returns
/// `VerifyError::Mismatch` if the HMAC does not equal the header value.
pub fn verify(
    secret: &SecretString,
    timestamp: SlackEventTimestamp,
    signature: &SlackSignature,
    raw_body: &[u8],
    now: DateTime<Utc>,
) -> Result<(), VerifyError> {
    // Gate 1: freshness. Use absolute skew so backwards-running clocks
    // are also rejected.
    let skew_secs = (now.timestamp() - timestamp.get()).unsigned_abs();
    let max_secs = SLACK_TIMESTAMP_MAX_SKEW.as_secs();
    if skew_secs > max_secs {
        return Err(VerifyError::StaleTimestamp { max_secs });
    }

    // Gate 2: HMAC. base = "v0:" + ts + ":" + body. We build into a
    // single allocation via the HMAC updater to avoid string churn.
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
        .map_err(|_| VerifyError::MalformedSignature("hmac key init"))?;
    mac.update(b"v0:");
    mac.update(timestamp.get().to_string().as_bytes());
    mac.update(b":");
    mac.update(raw_body);
    let computed = mac.finalize().into_bytes();

    // Header tail is hex(64) — decode into the same 32-byte buffer for
    // constant-time compare. We validate the tail's hex shape in the
    // newtype constructor, so this decode is infallible in practice.
    let mut expected = [0u8; 32];
    super::hex::decode_32(&signature.as_str()[3..], &mut expected)
        .map_err(|()| VerifyError::MalformedSignature("non-hex tail"))?;

    if computed.ct_eq(&expected).into() {
        Ok(())
    } else {
        Err(VerifyError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use hmac::Mac;

    fn make_secret() -> SecretString {
        SecretString::try_from("8f742231b10e8888abcd99yyyzzz85a5".to_string()).expect("secret")
    }

    /// Compute the canonical signature for a `(secret, ts, body)` triple.
    fn sign(secret: &SecretString, ts: i64, body: &[u8]) -> SlackSignature {
        use std::fmt::Write as _;
        let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes()).expect("key");
        mac.update(b"v0:");
        mac.update(ts.to_string().as_bytes());
        mac.update(b":");
        mac.update(body);
        let out = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(67);
        hex.push_str("v0=");
        for byte in out {
            write!(hex, "{byte:02x}").expect("invariant: write to in-memory String is infallible");
        }
        SlackSignature::try_from(hex).expect("valid sig")
    }

    #[test]
    fn accepts_valid_signature_within_skew() {
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = br#"{"type":"url_verification"}"#;
        let sig = sign(&secret, ts, body);
        let now = Utc.timestamp_opt(ts + 30, 0).single().expect("ts");
        assert!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_stale_timestamp_in_the_past() {
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = b"hello";
        let sig = sign(&secret, ts, body);
        // 6 minutes ahead → outside the 5-minute window.
        let now = Utc.timestamp_opt(ts + 6 * 60, 0).single().expect("ts");
        assert_eq!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            ),
            Err(VerifyError::StaleTimestamp { max_secs: 300 })
        );
    }

    #[test]
    fn rejects_stale_timestamp_in_the_future() {
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = b"hello";
        let sig = sign(&secret, ts, body);
        // Client clock 6 minutes ahead → backwards skew.
        let now = Utc.timestamp_opt(ts - 6 * 60, 0).single().expect("ts");
        assert!(matches!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            ),
            Err(VerifyError::StaleTimestamp { .. })
        ));
    }

    #[test]
    fn rejects_mismatched_signature() {
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = b"hello";
        let sig = sign(&secret, ts, body);
        // Verify against a different body → signature does not match.
        let now = Utc.timestamp_opt(ts, 0).single().expect("ts");
        assert_eq!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                b"different",
                now
            ),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn rejects_signature_made_with_different_secret() {
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = b"hello";
        let other =
            SecretString::try_from("a-completely-different-secret".to_string()).expect("secret");
        let sig = sign(&other, ts, body);
        let now = Utc.timestamp_opt(ts, 0).single().expect("ts");
        assert_eq!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            ),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn accepts_empty_body_with_correct_mac() {
        // Slack's `url_verification` ack edge case: body is non-empty but
        // some test fixtures pass an empty body — the verifier still must
        // succeed when the signature matches what was computed.
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body: &[u8] = b"";
        let sig = sign(&secret, ts, body);
        let now = Utc.timestamp_opt(ts, 0).single().expect("ts");
        assert!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            )
            .is_ok()
        );
    }

    #[test]
    fn skew_at_exact_max_is_accepted() {
        // Boundary: skew == max_secs is OK, > is not.
        let secret = make_secret();
        let ts = 1_700_000_000;
        let body = b"x";
        let sig = sign(&secret, ts, body);
        let now = Utc.timestamp_opt(ts + 300, 0).single().expect("ts");
        assert!(
            verify(
                &secret,
                SlackEventTimestamp::from_epoch_secs(ts).expect("ts"),
                &sig,
                body,
                now
            )
            .is_ok()
        );
    }
}
