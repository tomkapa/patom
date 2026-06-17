//! Signed-token mint + verify for the Lark MCP connect link.
//!
//! Lark's long-connection delivers events only — it cannot host an
//! interactive card, so an agent's `WireMcpRequest` renders as a plain-text
//! message with a `GET /lark/mcp/connect?token=…` URL. That endpoint runs
//! without a session cookie; auth is the signed token. The payload binds the
//! catalog being wired, the **already-resolved** Patom `(org_id, user_id)`
//! that owns the wiring (Lark shadow-mints the sender at inbound, so there is
//! no separate link table to re-resolve against — unlike Slack), the Lark
//! chat the request came from (so the OAuth callback can post the
//! "✓ Connected" ping back), and the agent + Patom thread that emitted the
//! request (so the universal auto-continue resumes the right agent loop).
//!
//! Pattern: HMAC-SHA256 over a `:`-separated payload, hex-encoded — the same
//! shape as [`crate::slack::connect_link`], over a key derived from the
//! deployment's `master_kek` (see `app.rs`).
//!
//! ## Wire shape
//!
//! ```text
//! <catalog>:<org>:<user>:<app>:<chat>:<reply>:<thread_uuid>:<agent_uuid>:<exp>:<hex_sig>
//! ```
//!
//! Every field is `:`-free: catalog id and Lark ids are `[A-Za-z0-9_-]`,
//! org/user/thread/agent are hyphenated UUIDs, `exp` is digits. `<reply>` is
//! the optional reply-anchor message id, rendered as an **empty segment**
//! when absent (an empty string is never a valid Lark id, so the split stays
//! unambiguous).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::hex;
use crate::mcp::McpCatalogId;
use crate::threads::ThreadId;

use super::types::{LarkAppId, LarkChatId, LarkMessageId};

type HmacSha256 = Hmac<Sha256>;

/// Payload encoded into the signed token. Constructed by the stream pump when
/// it sees a `WireMcpRequest` chunk; consumed by the `GET /lark/mcp/connect`
/// handler after verifying the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkConnectClaims {
    pub catalog_id: McpCatalogId,
    pub org_id: OrgId,
    pub user_id: UserId,
    pub app_id: LarkAppId,
    pub chat_id: LarkChatId,
    pub reply_to: Option<LarkMessageId>,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

/// Mint a signed token. `exp_secs` is an absolute Unix-epoch second value;
/// `verify_connect` rejects any token whose `exp` is below `now_secs`.
#[must_use]
pub fn sign_connect(key: &[u8], claims: &LarkConnectClaims, exp_secs: i64) -> String {
    let payload = render_payload(claims, exp_secs);
    let mut mac = HmacSha256::new_from_slice(key).expect("invariant: signing key non-empty");
    mac.update(payload.as_bytes());
    let hex_sig = hex::encode_32(&mac.finalize().into_bytes());
    format!("{payload}:{hex_sig}")
}

/// Verify + parse a token. Returns `None` for any malformation, expired `exp`,
/// or signature mismatch — the caller renders an error page on `None` without
/// distinguishing causes.
#[must_use]
pub fn verify_connect(key: &[u8], token: &str, now_secs: i64) -> Option<LarkConnectClaims> {
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

    // Parse `catalog:org:user:app:chat:reply:thread:agent:exp`.
    let mut parts = payload.split(':');
    let catalog_raw = parts.next()?;
    let org_raw = parts.next()?;
    let user_raw = parts.next()?;
    let app_raw = parts.next()?;
    let chat_raw = parts.next()?;
    let reply_raw = parts.next()?;
    let thread_raw = parts.next()?;
    let agent_raw = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        // Trailing fields → corruption.
        return None;
    }

    let catalog_id = McpCatalogId::try_from(catalog_raw).ok()?;
    let org_id = Uuid::parse_str(org_raw).ok().map(OrgId::from)?;
    let user_id = Uuid::parse_str(user_raw).ok().map(UserId::from)?;
    let app_id = LarkAppId::try_from(app_raw).ok()?;
    let chat_id = LarkChatId::try_from(chat_raw).ok()?;
    // Empty segment → no reply anchor; a present one must parse as a Lark id.
    let reply_to = if reply_raw.is_empty() {
        None
    } else {
        Some(LarkMessageId::try_from(reply_raw).ok()?)
    };
    let thread_id = Uuid::parse_str(thread_raw).ok().map(ThreadId::from)?;
    let agent_id = Uuid::parse_str(agent_raw).ok().map(AgentId::from)?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }

    Some(LarkConnectClaims {
        catalog_id,
        org_id,
        user_id,
        app_id,
        chat_id,
        reply_to,
        thread_id,
        agent_id,
    })
}

fn render_payload(claims: &LarkConnectClaims, exp_secs: i64) -> String {
    format!(
        "{catalog}:{org}:{user}:{app}:{chat}:{reply}:{thread}:{agent}:{exp}",
        catalog = claims.catalog_id.as_str(),
        org = claims.org_id.as_uuid(),
        user = claims.user_id.as_uuid(),
        app = claims.app_id.as_str(),
        chat = claims.chat_id.as_str(),
        reply = claims.reply_to.as_ref().map_or("", LarkMessageId::as_str),
        thread = claims.thread_id.as_uuid(),
        agent = claims.agent_id.as_uuid(),
        exp = exp_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims() -> LarkConnectClaims {
        LarkConnectClaims {
            catalog_id: McpCatalogId::try_from("notion").expect("valid catalog id"),
            org_id: OrgId::new(),
            user_id: UserId::new(),
            app_id: LarkAppId::try_from("cli_a1b2c3d4e5f6").expect("valid app"),
            chat_id: LarkChatId::try_from("oc_chat123").expect("valid chat"),
            reply_to: Some(LarkMessageId::try_from("om_msg123").expect("valid msg")),
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
    fn roundtrip_without_reply_to() {
        let claims = LarkConnectClaims {
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
        let exp = 1_700_000_000;
        let token = sign_connect(TEST_KEY, &claims, exp);
        assert!(verify_connect(TEST_KEY, &token, exp + 1).is_none());
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
        // Right field count but bogus UUIDs.
        let bad = format!(
            "notion:not-a-uuid:also-bad:cli_app:oc_chat::not-a-uuid:nope:4102444800:{}",
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
