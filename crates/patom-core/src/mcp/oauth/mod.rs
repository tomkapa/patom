//! Upstream-vendor OAuth 2.0 plumbing for MCP servers — built on
//! `rmcp::transport::auth`.
//!
//! Patom delegates the whole OAuth protocol surface (discovery, PKCE
//! generation, DCR per RFC 7591, code exchange, refresh, bearer
//! injection on outgoing requests) to rmcp 1.7's `auth` feature, which
//! is the same machinery codex consumes
//! (`codex-rs/rmcp-client/src/perform_oauth_login.rs`). This module
//! owns the multi-tenant adapters between rmcp's traits and patom's
//! Postgres persistence:
//!
//!   * [`PatomCredentialStore`] — `impl rmcp::CredentialStore` keyed at
//!     construction by `(server_id, org_id)`. Wraps patom's encrypted
//!     `mcp_server_credentials` row; rmcp's `StoredCredentials` is the
//!     persisted payload verbatim.
//!   * [`PatomStateStore`] — `impl rmcp::StateStore` for the PKCE +
//!     CSRF state that bridges `/oauth/start` → `/oauth/callback`.
//!     Postgres-backed (`mcp_oauth_pending`) so the callback can land
//!     on any replica.
//!   * [`session`] — orchestration. [`start_authorization`] mirrors
//!     codex's branching on `client_source` (Platform vs DCR vs None).
//!     [`handle_callback`] resumes the flow, exchanges the code,
//!     persists tokens.
//!
//! No DCR / PKCE / token-exchange / refresh code lives here any more —
//! that's all rmcp.

mod credential_adapter;
mod errors;
mod session;
mod state_adapter;

pub use credential_adapter::PatomCredentialStore;
pub use errors::OAuthError;
pub use session::{
    ConnectCtx, StartCtx, build_manager_for_request, handle_callback, start_authorization,
};
pub use state_adapter::{
    PatomPendingCtx, PatomStateStore, PgMcpOAuthPendingStore, ResumeCtx,
    SharedMcpOAuthPendingStore, SlackPingCtx,
};
