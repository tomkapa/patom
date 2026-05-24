//! Storage seam for the OAuth tables. Two stores:
//!   - [`McpOAuthClientStore`] — per-(org, issuer) registered DCR clients,
//!     with encrypted `client_secret` + `registration_access_token`.
//!   - [`McpOAuthPendingStore`] — short-lived (state, server_id, …) rows
//!     bridging `POST /oauth/start` to `GET /oauth/callback`.
//!
//! Both surface only validated domain types; ciphertext + nonces stay
//! inside the impls.

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
/// Slack-side wiring (hypothetical future "/relay connect notion"
/// command without an active thread) might populate this alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPingCtx {
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: String,
}

crate::str_enum! {
    /// Token-endpoint authentication method as defined by RFC 7591 §2.
    /// The DB CHECK constraint and the wire format both key off these
    /// labels; adding a method is a one-line edit.
    pub enum TokenAuthMethod {
        None              => "none",
        ClientSecretBasic => "client_secret_basic",
        ClientSecretPost  => "client_secret_post",
    }
}

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

/// Where this OAuth client came from.
///
/// Drives `mcp_oauth_clients` write semantics: DCR rows are idempotent
/// (re-registering the same vendor for the same org must not churn the
/// row); operator-supplied rows replace in full (rotating a pasted
/// secret must actually overwrite ciphertext); shared rows live with
/// `org_id IS NULL` and are reused across every tenant.
///
/// `org_id` lives on each variant that needs one so the type makes the
/// `(provenance, org_id)` pairing unrepresentable rather than relying on
/// a runtime guard in the store impl — §1 in CLAUDE.md.
///
/// Adding a fourth provenance (e.g. "vendor-specific quirk that needs
/// extra refresh headers") is one new variant here plus one match arm
/// in the store impl — no trait churn.
#[derive(Debug, Clone)]
pub enum ClientProvenance {
    /// RFC 7591 Dynamic Client Registration response. Carries the
    /// management URL + bearer the AS returned, so we can in theory
    /// deregister (RFC 7592) later.
    Dcr {
        org_id: OrgId,
        registration_client_uri: Option<String>,
        registration_access_token: Option<SecretString>,
    },
    /// Operator pasted credentials they created out-of-band in the
    /// vendor's developer console (vendors that don't implement DCR).
    /// Never carries `registration_*` fields by construction.
    Operator { org_id: OrgId },
    /// Platform-operator-provisioned client, shared across every tenant
    /// of this Relay deployment. Persists with `org_id IS NULL`. Used
    /// for vendors that don't support DCR (Google, Microsoft 365 — same
    /// model Anthropic uses for Claude Desktop's Gmail connector).
    /// Written exclusively by the boot-time seeder; never overwritten
    /// by the per-org OAuth flows.
    Shared,
}

impl ClientProvenance {
    /// Owning org for this client, or `None` for `Shared` (the row
    /// lives with `org_id IS NULL`). Lets call sites that don't care
    /// about the provenance shape read the org seam uniformly.
    #[must_use]
    pub fn org_id(&self) -> Option<OrgId> {
        match self {
            Self::Dcr { org_id, .. } | Self::Operator { org_id } => Some(*org_id),
            Self::Shared => None,
        }
    }
}

/// New OAuth-client record ready to persist. Holds plaintext secrets
/// briefly — the store seals them before INSERT. The owning `org_id`
/// (when applicable) lives on [`ClientProvenance`].
#[derive(Debug, Clone)]
pub struct NewOAuthClient {
    pub issuer: String,
    pub client_id: OAuthClientId,
    pub client_secret: Option<SecretString>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub token_endpoint_auth_method: TokenAuthMethod,
    pub scope: Option<String>,
    pub provenance: ClientProvenance,
}

/// Decrypted OAuth client returned by a lookup.
///
/// `org_id` mirrors the row: `None` for shared platform rows, `Some` for
/// org-scoped (DCR or operator) rows. Downstream code reads the
/// authorize/token endpoints + credentials and does not branch on
/// org_id past the store seam.
#[derive(Debug, Clone)]
pub struct DcrClientRecord {
    pub org_id: Option<OrgId>,
    pub issuer: String,
    pub client_id: OAuthClientId,
    pub client_secret: Option<SecretString>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub token_endpoint_auth_method: TokenAuthMethod,
    pub scope: Option<String>,
}

#[async_trait]
pub trait McpOAuthClientStore: fmt::Debug + Send + Sync {
    /// Write-or-fetch dispatched by `new.provenance`:
    /// - [`ClientProvenance::Dcr`]: insert-or-return (existing row wins),
    ///   keyed by `(org_id, issuer)`.
    /// - [`ClientProvenance::Operator`]: full overwrite,
    ///   keyed by `(org_id, issuer)`; `registration_*` columns are
    ///   forced to NULL.
    /// - [`ClientProvenance::Shared`]: full overwrite,
    ///   keyed by `(issuer)` against the `org_id IS NULL` row.
    async fn upsert(&self, new: NewOAuthClient) -> Result<DcrClientRecord, OAuthError>;

    /// Lookup the org-scoped client for `(org_id, issuer)`. Returns
    /// `None` if no per-org row exists — the caller decides whether to
    /// fall through to [`Self::read_shared`] or run DCR.
    async fn read(
        &self,
        org_id: OrgId,
        issuer: &str,
    ) -> Result<Option<DcrClientRecord>, OAuthError>;

    /// Lookup the shared (platform-owned) client for `issuer`. Returns
    /// `None` when the seeder hasn't seeded an entry for that issuer.
    async fn read_shared(&self, issuer: &str) -> Result<Option<DcrClientRecord>, OAuthError>;
}

pub type SharedMcpOAuthClientStore = Arc<dyn McpOAuthClientStore>;

#[derive(Debug, Clone)]
pub struct PendingAuthorizationWrite {
    pub state: String,
    pub server_id: McpServerId,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub issuer: String,
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
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub state: String,
    pub server_id: McpServerId,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub issuer: String,
    pub pkce_verifier: String,
    pub redirect_to: Option<String>,
    pub resume_ctx: Option<ResumeCtx>,
    pub slack_ctx: Option<SlackPingCtx>,
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
