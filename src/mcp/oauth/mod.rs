//! Upstream-vendor OAuth 2.0 plumbing for MCP servers.
//!
//! Three concerns, one module:
//!   1. **Discovery** — given an MCP server URL, find the authorization
//!      server: probe `/.well-known/oauth-protected-resource` (RFC 9728),
//!      follow `authorization_servers[0]` and fetch
//!      `/.well-known/oauth-authorization-server` (RFC 8414).
//!   2. **Dynamic Client Registration** — register Relay as an OAuth
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
mod store;

pub use discovery::{AsMetadata, discover_authorization_server};
pub use errors::OAuthError;
pub use flow::{
    AuthorizeStart, OAuthFlowClient, PendingAuthorization, RefreshOutcome, TokenExchangeResult,
    build_authorize_url, exchange_code, refresh_oauth_token, register_dynamic_client,
};
pub use pg_store::{PgMcpOAuthClientStore, PgMcpOAuthPendingStore};
pub use refresher::{OAUTH_REFRESH_SKEW, OAuthRefresher, RefresherDeps, SharedOAuthTokenCache};
pub use store::{
    ClientProvenance, DcrClientRecord, McpOAuthClientStore, McpOAuthPendingStore, NewOAuthClient,
    OAuthClientId, PendingAuthorizationWrite, ResumeCtx, SharedMcpOAuthClientStore,
    SharedMcpOAuthPendingStore, SlackPingCtx, TokenAuthMethod,
};
