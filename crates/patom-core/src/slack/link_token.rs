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
///
/// `response_url` is the slash command's `response_url` so the completion
/// route can swap the original "Set up Patom" ephemeral for a success
/// message (`replace_original`). It is base64url-encoded on the wire (it
/// contains `:` and `/`, which would break the `:`-split payload) and is
/// signed, so the completion route never POSTs to an attacker-supplied
/// URL. Empty when no usable response_url was available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackLinkClaims {
    pub team_id: SlackTeamId,
    pub slack_user_id: SlackUserId,
    pub response_url: String,
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
    let rurl_b64 = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let team_id = SlackTeamId::try_from(team_raw).ok()?;
    let slack_user_id = SlackUserId::try_from(user_raw).ok()?;
    let response_url = decode_response_url(rurl_b64)?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }

    Some(SlackLinkClaims {
        team_id,
        slack_user_id,
        response_url,
    })
}

fn render_payload(claims: &SlackLinkClaims, exp_secs: i64) -> String {
    use base64::Engine as _;
    let rurl_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.response_url.as_bytes());
    format!(
        "{team}:{user}:{rurl}:{exp}",
        team = claims.team_id.as_str(),
        user = claims.slack_user_id.as_str(),
        rurl = rurl_b64,
        exp = exp_secs,
    )
}

/// Decode the base64url response_url field. An empty field (the "no
/// response_url" case) decodes to an empty string; a malformed or
/// non-UTF-8 field fails the whole token.
fn decode_response_url(rurl_b64: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(rurl_b64)
        .ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> SlackLinkClaims {
        SlackLinkClaims {
            team_id: SlackTeamId::try_from("T0ABCDE").expect("valid team"),
            slack_user_id: SlackUserId::try_from("U0USER1").expect("valid user"),
            // A realistic response_url — contains `:` and `/`, which the
            // base64url field must survive without breaking the split.
            response_url: "https://hooks.slack.com/commands/T0ABCDE/123/abcDEF".to_owned(),
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
    fn roundtrip_with_empty_response_url() {
        let claims = SlackLinkClaims {
            response_url: String::new(),
            ..sample_claims()
        };
        let token = sign_link(TEST_KEY, &claims, 4_102_444_800);
        let parsed = verify_link(TEST_KEY, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed.response_url, "");
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
