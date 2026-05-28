//! Upstream-vendor OAuth 2.0 plumbing for MCP servers.
//!
//! Three concerns, one module:
//!   1. **Discovery** — given an MCP server URL, find the authorization
//!      server: protected-resource metadata (RFC 9728) →
//!      authorization-server metadata (RFC 8414). One fetch each, no
//!      fallback probes, no delegated-issuer chase. Codex-shape.
//!   2. **Client resolution** — for a catalog entry, produce the
//!      `(client_id, client_secret, endpoints, auth_method)` tuple the
//!      flow needs. Platform entries read env; DCR entries register
//!      against the AS. The result is carried across `POST /oauth/start`
//!      → `GET /oauth/callback` on the pending row and then folded into
//!      the encrypted `OAuth2Payload` so one row per server holds
//!      everything refresh needs.
//!   3. **Browser flow** — mint PKCE + state, build the authorize URL,
//!      handle the callback by exchanging the code for tokens. The
//!      transport adapter ([`PatomMcpHttpClient`]) keeps tokens fresh on
//!      every request (refresh-on-acquire + refresh-on-401) — no
//!      background refresher.

mod client_resolver;
mod discovery;
mod errors;
mod flow;
mod pg_store;
mod store;
mod transport;

pub use client_resolver::{ResolveCtx, resolve as resolve_oauth_client_creds};
pub use discovery::{AsMetadata, discover_authorization_server};
pub use errors::OAuthError;
pub use flow::{
    AuthorizeStart, OAuthFlowClient, RefreshCreds, TokenExchangeResult, build_authorize_url,
    exchange_code,
};
pub use pg_store::PgMcpOAuthPendingStore;
pub use store::{
    OAuthClientCreds, PendingAuthorization, PendingAuthorizationWrite, PendingDcrClient, ResumeCtx,
    SharedMcpOAuthPendingStore, SlackPingCtx, TokenAuthMethod,
};
pub use transport::{PatomMcpHttpClient, PatomMcpHttpClientConfig};
