//! Upstream-vendor OAuth 2.0 plumbing for MCP servers.
//!
//! Three concerns, one module:
//!   1. **Discovery** — given an MCP server URL, find the authorization
//!      server: probe `/.well-known/oauth-protected-resource` (RFC 9728),
//!      follow `authorization_servers[0]` and fetch
//!      `/.well-known/oauth-authorization-server` (RFC 8414).
//!   2. **Dynamic Client Registration** — register Patom as an OAuth
//!      client against the discovered AS via RFC 7591, store the
//!      resulting `client_id` and encrypted `client_secret` per
//!      `(org_id, issuer)` so subsequent flows reuse them.
//!   3. **Browser flow** — mint PKCE + state, build the authorize URL,
//!      handle the callback by exchanging the code for tokens, and write
//!      the token payload through the credentials seam.
//!
//! Refresh + on-call refresh-and-retry land in phase D.

mod discovery;
mod errors;
mod flow;
mod pg_store;
mod refresher;
mod shared_seed;
mod store;

pub use discovery::{AsMetadata, discover_authorization_server};
pub use errors::OAuthError;
pub use flow::{
    AuthorizeStart, OAuthFlowClient, PendingAuthorization, RefreshOutcome, TokenExchangeResult,
    build_authorize_url, exchange_code, refresh_oauth_token, register_dynamic_client,
};
pub use pg_store::{PgMcpOAuthClientStore, PgMcpOAuthPendingStore};
pub use refresher::{OAUTH_REFRESH_SKEW, OAuthRefresher, RefresherDeps, SharedOAuthTokenCache};
pub use shared_seed::seed_shared_clients;
pub use store::{
    ClientProvenance, DcrClientRecord, McpOAuthClientStore, McpOAuthPendingStore, NewOAuthClient,
    OAuthClientId, PendingAuthorizationWrite, ResumeCtx, SharedMcpOAuthClientStore,
    SharedMcpOAuthPendingStore, SlackPingCtx, TokenAuthMethod,
};

/// Read-only "find an existing OAuth client for `(org_id, issuer)`".
///
/// Encodes the canonical precedence used everywhere in the OAuth
/// subsystem: org-scoped row (operator-provisioned or prior DCR) wins,
/// shared platform row (`org_id IS NULL`, seeded by
/// [`seed_shared_clients`]) is the fallback.
///
/// Returns `None` only when neither lookup finds a row. The HTTP start
/// path additionally runs DCR on `None`; the callback's `load_dcr` and
/// the background refresher both treat `None` as a misconfiguration
/// because the start path is supposed to have minted the row already.
///
/// Centralised so the precedence cannot drift between call sites — a
/// missing fallback would silently break every shared-client (Gmail /
/// future M365) connection at refresh time.
pub async fn resolve_oauth_client(
    store: &SharedMcpOAuthClientStore,
    org_id: crate::auth::OrgId,
    issuer: &str,
) -> Result<Option<DcrClientRecord>, errors::OAuthError> {
    if let Some(row) = store.read(org_id, issuer).await? {
        return Ok(Some(row));
    }
    store.read_shared(issuer).await
}

/// Canonicalize an OAuth issuer for storage / comparison. RFC 8414 §2
/// requires the AS's `issuer` to round-trip exactly as published, but
/// real-world vendors are inconsistent about trailing slashes — Google's
/// AS metadata self-declares as `https://accounts.google.com` (no
/// slash) while its protected-resource document advertises the AS as
/// `https://accounts.google.com/` (with slash). A one-character drift
/// caused the shared-client lookup to miss and the flow to fall through
/// to DCR with `DcrUnsupported`. Strip the trailing slash on every
/// write, read, and vendor predicate so the two forms are equivalent
/// across the whole OAuth subsystem.
#[inline]
#[must_use]
pub(crate) fn canonical_issuer(raw: &str) -> &str {
    raw.strip_suffix('/').unwrap_or(raw)
}
