//! Signed-token mint + verify for the "Set up Patom" identity-link button.
//!
//! When an unlinked Slack user runs `/patom`, the slash response carries a
//! URL button to `GET /slack/identity/start?token=...`. That route runs
//! without a session cookie — auth is the signed token. The token binds
//! the Slack workspace + the Slack user who clicked, so the post-login
//! completion route can write the `slack_identities` link for exactly that
//! `(team, slack_user)` once the user has authenticated to Patom.
//!
//! Pattern: HMAC-SHA256 over a `:`-separated payload, hex-encoded — the
//! same shape as [`super::connect_link`] / [`super::oauth::sign_state`],
//! reusing the same `signing_secret`, the 10-minute TTL, and the
//! constant-time comparison.
//!
//! ## Wire shape
//!
//! ```text
//! <team>:<slack_user>:<exp>:<hex_sig>
//! ```
//!
//! Both `team` and `slack_user` are Slack ids (alphanumeric + `_-`) and
//! `exp` is digits — none contain `:`, so the split is unambiguous.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::types::{SlackTeamId, SlackUserId};

type HmacSha256 = Hmac<Sha256>;

/// Default TTL for an identity-link token, in seconds.
///
/// Matches the Slack install-state and MCP-connect tokens (10 minutes) —
/// long enough for a browser OAuth round trip, short enough that a leaked
/// button URL is useless minutes later.
pub const LINK_TOKEN_TTL_SECS: i64 = 600;

/// Payload encoded into the signed link token.
///
/// Minted by the `/patom` slash handler; consumed by
/// `GET /slack/identity/start` and re-verified by
/// `GET /slack/identity/complete` after login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackLinkClaims {
    pub team_id: SlackTeamId,
    pub slack_user_id: SlackUserId,
}

/// Mint a signed token. `exp_secs` is an absolute Unix-epoch second value;
/// [`verify_link`] rejects any token whose `exp` is below `now_secs`.
#[must_use]
pub fn sign_link(key: &[u8], claims: &SlackLinkClaims, exp_secs: i64) -> String {
    let payload = render_payload(claims, exp_secs);
    let mut mac = HmacSha256::new_from_slice(key).expect("invariant: signing key non-empty");
    mac.update(payload.as_bytes());
    let hex_sig = super::hex::encode_32(&mac.finalize().into_bytes());
    format!("{payload}:{hex_sig}")
}

/// Verify + parse a token. Returns `None` for any malformation, expired
/// `exp`, or signature mismatch — the caller renders an error page on
/// `None` without distinguishing causes.
#[must_use]
pub fn verify_link(key: &[u8], token: &str, now_secs: i64) -> Option<SlackLinkClaims> {
    let (payload, sig_hex) = token.rsplit_once(':')?;
    if sig_hex.len() != 64 {
        return None;
    }

    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();
    let mut expected = [0u8; 32];
    super::hex::decode_32(sig_hex, &mut expected).ok()?;
    if !bool::from(computed.ct_eq(&expected)) {
        return None;
    }

    let mut parts = payload.split(':');
    let team_raw = parts.next()?;
    let user_raw = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let team_id = SlackTeamId::try_from(team_raw).ok()?;
    let slack_user_id = SlackUserId::try_from(user_raw).ok()?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }

    Some(SlackLinkClaims {
        team_id,
        slack_user_id,
    })
}

fn render_payload(claims: &SlackLinkClaims, exp_secs: i64) -> String {
    format!(
        "{team}:{user}:{exp}",
        team = claims.team_id.as_str(),
        user = claims.slack_user_id.as_str(),
        exp = exp_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> SlackLinkClaims {
        SlackLinkClaims {
            team_id: SlackTeamId::try_from("T0ABCDE").expect("valid team"),
            slack_user_id: SlackUserId::try_from("U0USER1").expect("valid user"),
        }
    }

    const TEST_KEY: &[u8] = b"test-signing-key-must-be-non-empty-and-stable";

    #[test]
    fn roundtrip_succeeds() {
        let claims = sample_claims();
        let exp = 4_102_444_800; // year 2100
        let token = sign_link(TEST_KEY, &claims, exp);
        let parsed = verify_link(TEST_KEY, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed, claims);
    }

    #[test]
    fn rejects_expired() {
        let claims = sample_claims();
        let exp = 1_700_000_000;
        let token = sign_link(TEST_KEY, &claims, exp);
        assert!(verify_link(TEST_KEY, &token, exp + 1).is_none());
    }

    #[test]
    fn accepts_exactly_at_expiry_boundary() {
        let claims = sample_claims();
        let exp = 1_700_000_000;
        let token = sign_link(TEST_KEY, &claims, exp);
        // exp == now is still valid; only exp < now is rejected.
        assert!(verify_link(TEST_KEY, &token, exp).is_some());
    }

    #[test]
    fn rejects_tampered_payload() {
        let claims = sample_claims();
        let token = sign_link(TEST_KEY, &claims, 4_102_444_800);
        let mut bytes: Vec<char> = token.chars().collect();
        bytes[0] = if bytes[0] == 'A' { 'B' } else { 'A' };
        let bad: String = bytes.into_iter().collect();
        assert!(verify_link(TEST_KEY, &bad, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_tampered_signature() {
        let claims = sample_claims();
        let mut token = sign_link(TEST_KEY, &claims, 4_102_444_800);
        let last = token.pop().expect("non-empty");
        token.push(if last == 'a' { 'b' } else { 'a' });
        assert!(verify_link(TEST_KEY, &token, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_wrong_key() {
        let k1: &[u8] = b"key-one-must-be-non-empty-aaaaaaaaaaa";
        let k2: &[u8] = b"key-two-must-be-non-empty-bbbbbbbbbbb";
        let claims = sample_claims();
        let token = sign_link(k1, &claims, 4_102_444_800);
        assert!(verify_link(k2, &token, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_malformed_token() {
        assert!(verify_link(TEST_KEY, "", 1_700_000_000).is_none());
        assert!(verify_link(TEST_KEY, "nodots", 1_700_000_000).is_none());
        // Right field count but signature too short.
        assert!(verify_link(TEST_KEY, "T0ABCDE:U0USER1:4102444800:short", 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_trailing_garbage_after_payload() {
        // A valid signature over an over-long payload must still be rejected
        // by the field-count guard in parse().
        let claims = sample_claims();
        let payload = render_payload(&claims, 4_102_444_800);
        let augmented_payload = format!("{payload}:extra");
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).expect("key");
        mac.update(augmented_payload.as_bytes());
        let hex_sig = super::super::hex::encode_32(&mac.finalize().into_bytes());
        let token = format!("{augmented_payload}:{hex_sig}");
        assert!(verify_link(TEST_KEY, &token, 1_700_000_000).is_none());
    }
}
