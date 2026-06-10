//! Signed-token mint + verify for the Slack Block Kit Connect button.
//!
//! The button's `url` field is a `GET /slack/mcp/connect?token=...` that
//! runs without a session cookie — auth is the signed token. Token
//! payload binds the originating Slack thread, the Slack user who saw
//! the card, the catalog entry being wired, and the agent + Patom thread
//! that emitted the request (so the OAuth callback can resume the
//! agent loop after the user finishes consent).
//!
//! Pattern: HMAC-SHA256 over a `:`-separated payload, hex-encoded.
//! Mirrors [`super::oauth::sign_state`] / `verify_state` exactly —
//! same `signing_secret`, same TTL (10 min), same constant-time
//! comparison.
//!
//! ## Wire shape
//!
//! ```text
//! <catalog>:<team>:<channel>:<thread>:<slack_user>:<thread_uuid>:<agent_uuid>:<exp>:<hex_sig>
//! ```
//!
//! `:` is the field separator because every payload field is constrained
//! to one of:
//!   - alphanumeric + `_-` (slack ids, catalog id)
//!   - digits + `.` (thread_ts)
//!   - hyphenated UUIDs (thread, agent)
//!   - digits (exp)
//!
//! none of which contain `:`, so the split is unambiguous.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::mcp::McpCatalogId;
use crate::threads::ThreadId;

use super::types::{SlackChannelId, SlackTeamId, SlackThreadTs, SlackUserId};

type HmacSha256 = Hmac<Sha256>;

/// Payload encoded into the signed token. Constructed by the stream
/// pump when it sees a `WireMcpRequest` chunk; consumed by the
/// `GET /slack/mcp/connect` handler after verifying the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConnectClaims {
    pub catalog_id: McpCatalogId,
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub thread_ts: SlackThreadTs,
    pub slack_user_id: SlackUserId,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

/// Mint a signed token. `exp_secs` is an absolute Unix-epoch second value;
/// `verify_connect` rejects any token whose `exp` is below `now_secs`.
#[must_use]
pub fn sign_connect(key: &[u8], claims: &SlackConnectClaims, exp_secs: i64) -> String {
    let payload = render_payload(claims, exp_secs);
    let mut mac = HmacSha256::new_from_slice(key).expect("invariant: signing key non-empty");
    mac.update(payload.as_bytes());
    let hex_sig = super::hex::encode_32(&mac.finalize().into_bytes());
    format!("{payload}:{hex_sig}")
}

/// Verify + parse a token. Returns `None` for any malformation, expired
/// `exp`, or signature mismatch — the caller should respond 400 / render
/// an error page on `None` without distinguishing causes.
#[must_use]
pub fn verify_connect(key: &[u8], token: &str, now_secs: i64) -> Option<SlackConnectClaims> {
    // Split off the trailing `:<hex_sig>`.
    let (payload, sig_hex) = token.rsplit_once(':')?;
    if sig_hex.len() != 64 {
        return None;
    }

    // Recompute MAC.
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();
    let mut expected = [0u8; 32];
    super::hex::decode_32(sig_hex, &mut expected).ok()?;
    if !bool::from(computed.ct_eq(&expected)) {
        return None;
    }

    // Parse `catalog:team:channel:thread:user:session:agent:exp`.
    let mut parts = payload.split(':');
    let catalog_raw = parts.next()?;
    let team_raw = parts.next()?;
    let channel_raw = parts.next()?;
    let thread_raw = parts.next()?;
    let user_raw = parts.next()?;
    let thread_raw_uuid = parts.next()?;
    let agent_raw = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        // Trailing fields → corruption.
        return None;
    }

    let catalog_id = McpCatalogId::try_from(catalog_raw).ok()?;
    let team_id = SlackTeamId::try_from(team_raw).ok()?;
    let channel_id = SlackChannelId::try_from(channel_raw).ok()?;
    let thread_ts = SlackThreadTs::try_from(thread_raw).ok()?;
    let slack_user_id = SlackUserId::try_from(user_raw).ok()?;
    let thread_id = Uuid::parse_str(thread_raw_uuid).ok().map(ThreadId::from)?;
    let agent_id = Uuid::parse_str(agent_raw).ok().map(AgentId::from)?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }

    Some(SlackConnectClaims {
        catalog_id,
        team_id,
        channel_id,
        thread_ts,
        slack_user_id,
        thread_id,
        agent_id,
    })
}

fn render_payload(claims: &SlackConnectClaims, exp_secs: i64) -> String {
    format!(
        "{catalog}:{team}:{channel}:{thread}:{user}:{thread_id}:{agent}:{exp}",
        catalog = claims.catalog_id.as_str(),
        team = claims.team_id.as_str(),
        channel = claims.channel_id.as_str(),
        thread = claims.thread_ts.as_str(),
        user = claims.slack_user_id.as_str(),
        thread_id = claims.thread_id.as_uuid(),
        agent = claims.agent_id.as_uuid(),
        exp = exp_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> SlackConnectClaims {
        SlackConnectClaims {
            catalog_id: McpCatalogId::try_from("notion").expect("valid catalog id"),
            team_id: SlackTeamId::try_from("T0ABCDE").expect("valid team"),
            channel_id: SlackChannelId::try_from("C1XYZ12").expect("valid channel"),
            thread_ts: SlackThreadTs::try_from("1700000000.000100").expect("valid thread"),
            slack_user_id: SlackUserId::try_from("U0USER1").expect("valid user"),
            thread_id: ThreadId::new(),
            agent_id: AgentId::new(),
        }
    }

    const TEST_KEY: &[u8] = b"test-signing-key-must-be-non-empty-and-stable";

    #[test]
    fn roundtrip_succeeds() {
        let claims = sample_claims();
        let exp = 4_102_444_800; // year 2100
        let token = sign_connect(TEST_KEY, &claims, exp);
        let parsed = verify_connect(TEST_KEY, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed, claims);
    }

    #[test]
    fn rejects_expired() {
        let claims = sample_claims();
        let exp = 1_700_000_000;
        let token = sign_connect(TEST_KEY, &claims, exp);
        // now > exp → expired.
        assert!(verify_connect(TEST_KEY, &token, exp + 1).is_none());
    }

    #[test]
    fn rejects_tampered_payload() {
        let claims = sample_claims();
        let token = sign_connect(TEST_KEY, &claims, 4_102_444_800);
        // Flip the catalog_id segment.
        let mut bytes: Vec<char> = token.chars().collect();
        // first char of payload is the first char of catalog id.
        bytes[0] = if bytes[0] == 'a' { 'b' } else { 'a' };
        let bad: String = bytes.into_iter().collect();
        assert!(verify_connect(TEST_KEY, &bad, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_tampered_signature() {
        let claims = sample_claims();
        let mut token = sign_connect(TEST_KEY, &claims, 4_102_444_800);
        let last = token.pop().expect("non-empty");
        token.push(if last == 'a' { 'b' } else { 'a' });
        assert!(verify_connect(TEST_KEY, &token, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_wrong_key() {
        let k1: &[u8] = b"key-one-must-be-non-empty-aaaaaaaaaaa";
        let k2: &[u8] = b"key-two-must-be-non-empty-bbbbbbbbbbb";
        let claims = sample_claims();
        let token = sign_connect(k1, &claims, 4_102_444_800);
        assert!(verify_connect(k2, &token, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_malformed_token() {
        assert!(verify_connect(TEST_KEY, "", 1_700_000_000).is_none());
        assert!(verify_connect(TEST_KEY, "nodots", 1_700_000_000).is_none());
        // Sig length wrong.
        assert!(verify_connect(TEST_KEY, "a:b:c:d:e:f:g:h:short", 1_700_000_000).is_none());
        // Right shape but bogus UUIDs.
        let bad = format!(
            "notion:T0ABCDE:C1XYZ12:1700000000.000100:U0USER1:not-a-uuid:also-not:4102444800:{}",
            "0".repeat(64),
        );
        assert!(verify_connect(TEST_KEY, &bad, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_trailing_garbage_after_payload() {
        // An attacker appending an extra colon-segment must not be accepted —
        // even with a valid signature for the original payload, parse() rejects
        // the trailing field.
        let claims = sample_claims();
        let payload = render_payload(&claims, 4_102_444_800);
        let augmented_payload = format!("{payload}:extra");
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).expect("key");
        mac.update(augmented_payload.as_bytes());
        let hex_sig = super::super::hex::encode_32(&mac.finalize().into_bytes());
        let token = format!("{augmented_payload}:{hex_sig}");
        assert!(verify_connect(TEST_KEY, &token, 1_700_000_000).is_none());
    }
}
