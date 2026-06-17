//! Signed-token mint + verify for the Discord MCP connect link.
//!
//! The Discord poster sends plain messages (no interactive components), so an
//! agent's `WireMcpRequest` renders as a plain-text message with a
//! `GET /discord/mcp/connect?token=…` URL. That endpoint runs without a
//! session cookie; auth is the signed token. The payload binds the catalog
//! being wired, the **already-resolved** Patom `(org_id, user_id)` that owns
//! the wiring (Discord shadow-mints the sender at inbound — no link table to
//! re-resolve), the Discord channel the request came from (so the OAuth
//! callback can post the "✓ Connected" ping back), and the agent + Patom
//! thread that emitted the request (so the universal auto-continue resumes the
//! right agent loop).
//!
//! Pattern: HMAC-SHA256 over a `:`-separated payload, hex-encoded — the same
//! shape as [`crate::slack::connect_link`] / [`crate::lark::connect_link`],
//! over a key derived from the deployment's `master_kek` (see `app.rs`).
//!
//! ## Wire shape
//!
//! ```text
//! <catalog>:<org>:<user>:<app>:<container>:<reply>:<thread_uuid>:<agent_uuid>:<exp>:<hex_sig>
//! ```
//!
//! Every field is `:`-free: catalog id is `[a-z0-9_-]`, Discord ids are
//! decimal snowflakes, org/user/thread/agent are hyphenated UUIDs, `exp` is
//! digits. `<reply>` is the optional reply-anchor message id, rendered as an
//! **empty segment** when absent.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::hex;
use crate::mcp::McpCatalogId;
use crate::threads::ThreadId;

use super::types::{ApplicationId, ContainerId, DiscordMessageId};

type HmacSha256 = Hmac<Sha256>;

/// Payload encoded into the signed token. Constructed by the stream pump when
/// it sees a `WireMcpRequest` chunk; consumed by the
/// `GET /discord/mcp/connect` handler after verifying the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordConnectClaims {
    pub catalog_id: McpCatalogId,
    pub org_id: OrgId,
    pub user_id: UserId,
    pub application_id: ApplicationId,
    pub container_id: ContainerId,
    pub reply_to: Option<DiscordMessageId>,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

/// Mint a signed token. `exp_secs` is an absolute Unix-epoch second value;
/// `verify_connect` rejects any token whose `exp` is below `now_secs`.
#[must_use]
pub fn sign_connect(key: &[u8], claims: &DiscordConnectClaims, exp_secs: i64) -> String {
    let payload = render_payload(claims, exp_secs);
    let mut mac = HmacSha256::new_from_slice(key).expect("invariant: signing key non-empty");
    mac.update(payload.as_bytes());
    let hex_sig = hex::encode_32(&mac.finalize().into_bytes());
    format!("{payload}:{hex_sig}")
}

/// Verify + parse a token. Returns `None` for any malformation, expired `exp`,
/// or signature mismatch.
#[must_use]
pub fn verify_connect(key: &[u8], token: &str, now_secs: i64) -> Option<DiscordConnectClaims> {
    // Trust-boundary length cap (§5) before any parse / HMAC work.
    if token.len() > crate::mcp::wire_connect::CONNECT_TOKEN_MAX_BYTES {
        return None;
    }
    let (payload, sig_hex) = token.rsplit_once(':')?;
    if sig_hex.len() != 64 {
        return None;
    }

    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();
    let mut expected = [0u8; 32];
    hex::decode_32(sig_hex, &mut expected).ok()?;
    if !bool::from(computed.ct_eq(&expected)) {
        return None;
    }

    // Parse `catalog:org:user:app:container:reply:thread:agent:exp`.
    let mut parts = payload.split(':');
    let catalog_raw = parts.next()?;
    let org_raw = parts.next()?;
    let user_raw = parts.next()?;
    let app_raw = parts.next()?;
    let container_raw = parts.next()?;
    let reply_raw = parts.next()?;
    let thread_raw = parts.next()?;
    let agent_raw = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let catalog_id = McpCatalogId::try_from(catalog_raw).ok()?;
    let org_id = Uuid::parse_str(org_raw).ok().map(OrgId::from)?;
    let user_id = Uuid::parse_str(user_raw).ok().map(UserId::from)?;
    let application_id = ApplicationId::try_from(app_raw).ok()?;
    let container_id = ContainerId::try_from(container_raw).ok()?;
    let reply_to = if reply_raw.is_empty() {
        None
    } else {
        Some(DiscordMessageId::try_from(reply_raw).ok()?)
    };
    let thread_id = Uuid::parse_str(thread_raw).ok().map(ThreadId::from)?;
    let agent_id = Uuid::parse_str(agent_raw).ok().map(AgentId::from)?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }

    Some(DiscordConnectClaims {
        catalog_id,
        org_id,
        user_id,
        application_id,
        container_id,
        reply_to,
        thread_id,
        agent_id,
    })
}

fn render_payload(claims: &DiscordConnectClaims, exp_secs: i64) -> String {
    format!(
        "{catalog}:{org}:{user}:{app}:{container}:{reply}:{thread}:{agent}:{exp}",
        catalog = claims.catalog_id.as_str(),
        org = claims.org_id.as_uuid(),
        user = claims.user_id.as_uuid(),
        app = claims.application_id.as_str(),
        container = claims.container_id.as_str(),
        reply = claims
            .reply_to
            .as_ref()
            .map_or("", DiscordMessageId::as_str),
        thread = claims.thread_id.as_uuid(),
        agent = claims.agent_id.as_uuid(),
        exp = exp_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> DiscordConnectClaims {
        DiscordConnectClaims {
            catalog_id: McpCatalogId::try_from("notion").expect("valid catalog id"),
            org_id: OrgId::new(),
            user_id: UserId::new(),
            application_id: ApplicationId::try_from("111222333444555666").expect("valid app"),
            container_id: ContainerId::try_from("777888999000111222").expect("valid container"),
            reply_to: Some(DiscordMessageId::try_from("333444555666777888").expect("valid msg")),
            thread_id: ThreadId::new(),
            agent_id: AgentId::new(),
        }
    }

    const TEST_KEY: &[u8] = b"test-signing-key-must-be-non-empty-and-stable";

    #[test]
    fn roundtrip_succeeds() {
        let claims = sample_claims();
        let token = sign_connect(TEST_KEY, &claims, 4_102_444_800);
        let parsed = verify_connect(TEST_KEY, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed, claims);
    }

    #[test]
    fn roundtrip_without_reply_to() {
        let claims = DiscordConnectClaims {
            reply_to: None,
            ..sample_claims()
        };
        let token = sign_connect(TEST_KEY, &claims, 4_102_444_800);
        let parsed = verify_connect(TEST_KEY, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed.reply_to, None);
        assert_eq!(parsed, claims);
    }

    #[test]
    fn rejects_expired() {
        let claims = sample_claims();
        let token = sign_connect(TEST_KEY, &claims, 1_700_000_000);
        assert!(verify_connect(TEST_KEY, &token, 1_700_000_001).is_none());
    }

    #[test]
    fn rejects_tampered_payload() {
        let claims = sample_claims();
        let token = sign_connect(TEST_KEY, &claims, 4_102_444_800);
        let mut bytes: Vec<char> = token.chars().collect();
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
        assert!(verify_connect(TEST_KEY, "a:b:c:d:e:f:g:h:i:short", 1_700_000_000).is_none());
        // Right field count, bogus uuids + non-digit snowflakes.
        let bad = format!(
            "notion:not-a-uuid:bad:notdigits:nope::x:y:4102444800:{}",
            "0".repeat(64),
        );
        assert!(verify_connect(TEST_KEY, &bad, 1_700_000_000).is_none());
    }

    #[test]
    fn rejects_trailing_garbage_after_payload() {
        let claims = sample_claims();
        let payload = render_payload(&claims, 4_102_444_800);
        let augmented_payload = format!("{payload}:extra");
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).expect("key");
        mac.update(augmented_payload.as_bytes());
        let hex_sig = hex::encode_32(&mac.finalize().into_bytes());
        let token = format!("{augmented_payload}:{hex_sig}");
        assert!(verify_connect(TEST_KEY, &token, 1_700_000_000).is_none());
    }
}
