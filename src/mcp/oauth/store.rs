//! Storage seam for the OAuth pending table.
//!
//! Short-lived `(state, server_id, …)` rows bridge `POST /oauth/start` to
//! `GET /oauth/callback`. After the refactor, freshly-registered DCR
//! client material (client_id + encrypted client_secret + auth method +
//! endpoints) is carried on the same pending row so the start→callback
//! handoff doesn't need a separate `mcp_oauth_clients` table.
//!
//! All boundary-validated types live here; ciphertext + nonces stay
//! inside the [`super::pg_store`] impl.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::mcp::McpServerId;
use crate::session::SessionId;
use crate::types::SecretString;

use super::errors::OAuthError;

/// Channel-agnostic resume context for the OAuth callback.
///
/// When populated on a [`PendingAuthorization`], the callback enqueues
/// a synthetic continuation prompt ("I've connected <name>. Please
/// continue.") into `session_id` so the agent loop can resume without
/// the user typing anything. Set by any channel that drives the start
/// flow on behalf of an in-flight conversation (web UI, Slack adapter,
/// future Lark / Teams). Absent for manual "wire from the catalog
/// page" flows where there is no live conversation to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeCtx {
    pub session_id: SessionId,
    pub agent_id: AgentId,
}

/// Slack-channel context for the "✓ Connected" follow-up ping.
///
/// Posted into the originating thread after the callback succeeds.
/// Slack-only — never populated for the web flow. Independent of
/// [`ResumeCtx`]: a Slack-initiated flow populates both; a manual
/// Slack-side wiring (hypothetical future "/patom connect notion"
/// command without an active thread) might populate this alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPingCtx {
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: String,
}

// `TokenAuthMethod` moved to `crate::mcp::types` so `credentials.rs` can
// embed it in `OAuth2Payload` without creating a `credentials → oauth →
// credentials` dep cycle. Re-exported here for source-compatibility.
pub use crate::mcp::types::TokenAuthMethod;

/// Vendor-issued OAuth `client_id`. Parsed once at the boundary — the
/// length cap defends against arbitrary input being persisted into the
/// encrypted-credentials row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientId(String);

impl OAuthClientId {
    pub const MAX_BYTES: usize = 512;

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OAuthClientId {
    type Error = crate::types::ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(crate::types::ParseError::Empty { field: "client_id" });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(crate::types::ParseError::TooLong {
                field: "client_id",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

/// OAuth client credentials resolved for one flow.
///
/// Replaces the pre-refactor `DcrClientRecord`. Same field set, but the
/// shape is no longer storage-tagged — it's just "what the start /
/// callback / refresh path needs to talk to the AS." The resolver
/// produces this from env (Platform) or DCR (a fresh registration).
#[derive(Debug, Clone)]
pub struct OAuthClientCreds {
    pub issuer: String,
    pub client_id: OAuthClientId,
    pub client_secret: Option<SecretString>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub token_endpoint_auth_method: TokenAuthMethod,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAuthorizationWrite {
    pub state: String,
    pub server_id: McpServerId,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub pkce_verifier: String,
    pub redirect_to: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Channel-agnostic resume context. When `Some`, the callback
    /// enqueues a synthetic continuation prompt (universal
    /// auto-continue).
    pub resume_ctx: Option<ResumeCtx>,
    /// Slack-thread channel context. When `Some`, the callback posts a
    /// "✓ Connected — <Provider>" follow-up into that thread.
    pub slack_ctx: Option<SlackPingCtx>,
    /// DCR-issued client material to carry from start to callback —
    /// `None` for Platform entries (resolver derives those from env on
    /// callback too). Present iff the catalog entry's `client_source =
    /// 'dcr'` and the resolver registered fresh.
    pub dcr_client: Option<PendingDcrClient>,
}

/// DCR-issued client material persisted with the pending row.
///
/// Carries the `(client_id, client_secret, auth_method, endpoints)`
/// produced by the start handler so the callback can exchange code with
/// the same client. Plaintext at the boundary; the store seals the
/// secret with the org's KEK before INSERT.
#[derive(Debug, Clone)]
pub struct PendingDcrClient {
    pub client_id: OAuthClientId,
    pub client_secret: Option<SecretString>,
    pub token_endpoint_auth_method: TokenAuthMethod,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub state: String,
    pub server_id: McpServerId,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub pkce_verifier: String,
    pub redirect_to: Option<String>,
    pub resume_ctx: Option<ResumeCtx>,
    pub slack_ctx: Option<SlackPingCtx>,
    pub dcr_client: Option<PendingDcrClient>,
}

#[async_trait]
pub trait McpOAuthPendingStore: fmt::Debug + Send + Sync {
    async fn insert(&self, row: PendingAuthorizationWrite) -> Result<(), OAuthError>;

    /// One-shot: read + delete in a single statement so the row cannot
    /// be replayed even if the AS issues two callbacks for the same
    /// state. Returns `None` for unknown / expired states.
    async fn consume(
        &self,
        state: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<PendingAuthorization>, OAuthError>;
}

pub type SharedMcpOAuthPendingStore = Arc<dyn McpOAuthPendingStore>;
