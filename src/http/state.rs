use std::path::PathBuf;
use std::sync::Arc;

use sqlx::PgPool;

use crate::agents::SharedAgentStore;
use crate::assets::SharedAssetStore;
use crate::auth::{
    GoogleOAuth, JwtSigner, SharedOrgLanguageResolver, SharedOrgRuleResolver, SharedUserStore,
};
use crate::clock::SharedClock;
use crate::http::MembershipCache;
use crate::mcp::oauth::{OAuthFlowClient, SharedMcpOAuthClientStore, SharedMcpOAuthPendingStore};
use crate::mcp::{
    McpRefreshTrigger, SharedMcpCatalogStore, SharedMcpCredentialStore, SharedMcpServerStore,
    TestConnectRateLimiter,
};
use crate::memory::SharedMemoryStore;
use crate::orgs::{SharedMailer, SharedOrgStore};
use crate::prompts::Prompts;
use crate::runtime::{
    SharedDagBudget, SharedLeaseManager, SharedPromptQueue, SharedResponseSource,
    SharedThreadStream,
};
use crate::session::SharedSessionStore;
use crate::slack::SlackAppState;

/// Cheaply-cloneable container of every collaborator the HTTP routes need. The router
/// gets a single `AppState` and threads it through axum's extractors.
#[derive(Clone, Debug)]
pub struct AppState {
    pub queue: SharedPromptQueue,
    #[allow(dead_code)] // surfaced for future endpoints (lease admin) and Postgres parity.
    pub leases: SharedLeaseManager,
    pub responses: SharedResponseSource,
    pub sessions: SharedSessionStore,
    pub agents: SharedAgentStore,
    /// DAG turn-budget handle. Threaded through state so `send_message`
    /// can `bump_or_fail` and the worker's quiescence trigger can query
    /// liveness without re-constructing the impl.
    pub dag: SharedDagBudget,
    /// Operator-side memory access (doc/memory.md §1.9). HTTP routes
    /// under `/agents/{id}/memory*` read and mutate through this handle.
    pub memory_store: SharedMemoryStore,
    pub mcp_store: SharedMcpServerStore,
    /// MCP catalog (known integrations: notion, linear, …). Read by
    /// `GET /mcp-catalog` for the frontend connections page and by the
    /// `POST /mcp-servers {catalog_id}` short-form to look up the default
    /// transport / auth_kind.
    pub mcp_catalog: SharedMcpCatalogStore,
    /// Envelope-encrypted credential seam paired with `mcp_store`. CRUD
    /// handlers route header / bearer-token writes through this store; the
    /// registry refresher reads via it on every connect.
    pub mcp_credentials: SharedMcpCredentialStore,
    /// Send-half of the MCP refresh signal. Cheap to clone; CRUD handlers fire it
    /// after every write. The owning coordinator task lives on [`Server`].
    pub mcp_refresh: McpRefreshTrigger,
    /// Per-user rate limiter for `POST /mcp-servers/test-connect`. Process-wide
    /// singleton shared across all handlers.
    pub mcp_test_rate: TestConnectRateLimiter,
    /// Per-(org, issuer) registered DCR clients store.
    pub mcp_oauth_clients: SharedMcpOAuthClientStore,
    /// Pending-authorization rows that bridge `POST /oauth/start` →
    /// `GET /oauth/callback`.
    pub mcp_oauth_pending: SharedMcpOAuthPendingStore,
    /// HTTP client bundle that drives discovery / DCR / token exchange.
    pub mcp_oauth_flow: OAuthFlowClient,
    /// Public-facing base URL Patom tells vendors to redirect back to.
    /// E.g. `https://patom.example/mcp-oauth/callback` is built by
    /// appending the canonical path to this base.
    pub oauth_redirect_base: std::sync::Arc<str>,
    /// Origin of the SPA (e.g. `http://localhost:5173` in dev). When
    /// `Some`, prepended to the post-OAuth-callback redirect so the
    /// browser leaves the BE host. `None` for same-origin deployments.
    pub web_base_url: Option<std::sync::Arc<str>>,
    /// Fan-in DAG stream — `GET /threads/{id}/stream` subscribes here. The
    /// owning task is held by [`Server`]; this handle is cheap to clone.
    pub thread_stream: SharedThreadStream,
    /// Shared connection pool for threads-route SQL (channel feed + thread
    /// history). The trait surface for those queries is small enough to keep
    /// inline in the route module rather than spinning up another store
    /// abstraction; this field is the seam.
    pub pool: PgPool,
    /// JWT signer used by the auth middleware to verify cookies and by
    /// the OAuth callback route to mint them.
    pub jwt: JwtSigner,
    /// Google OAuth client — owns the redirect URL, client id/secret,
    /// and the HTTP exchanger.
    pub oauth: GoogleOAuth,
    /// Identity-table store.
    ///
    /// Used by the OAuth callback to upsert users + personal org, and
    /// by the auth middleware for membership lookups.
    pub users: SharedUserStore,
    /// Injected clock. Auth code uses this to stamp `oauth_login_states`
    /// expiry; per CLAUDE.md §11 nothing in app code calls
    /// `SystemTime::now` directly.
    pub clock: SharedClock,
    /// Whether to set the `Secure` flag on the session cookie. Off in
    /// local-dev (plain http://localhost), on in any prod-shaped
    /// deployment. Sourced from [`crate::config::AuthSettings`].
    pub cookie_secure: bool,
    /// `(user_id, org_id) → role` lookup cache. Cuts the per-request
    /// membership round-trip down to a Mutex lookup for repeat callers.
    pub memberships: Arc<MembershipCache>,
    /// Per-language prompt registry. Loaded once at startup; the OAuth
    /// callback reads it to seed the per-org default agent in the right
    /// language. The agent worker hits it on every turn through
    /// `AgentMemory`.
    pub prompts: Arc<Prompts>,
    /// Per-agent language lookup. The `PATCH /me/org/language` handler
    /// invalidates the cache here so a switch propagates to the next
    /// agent turn without waiting for TTL.
    pub language_resolver: SharedOrgLanguageResolver,
    /// Per-agent organization-rule lookup. The `PATCH /me/org/rule`
    /// handler invalidates this cache so a rule edit propagates to the
    /// next agent turn without waiting for TTL — same lifecycle as
    /// [`Self::language_resolver`].
    pub rule_resolver: SharedOrgRuleResolver,
    /// SPA dist path the `ServeDir` fallback reads from.
    pub web_dist: PathBuf,
    /// Slack adapter wiring — `Some` when `PATOM_SLACK_*` env vars are
    /// configured, `None` otherwise. Public webhook + OAuth handlers
    /// 404 cleanly when this is `None`, so deployments without Slack
    /// stay first-class.
    pub slack: Option<SlackAppState>,
    /// Object-storage seam for user avatars and MCP catalog icons.
    /// `Some` when `PATOM_R2_*` env vars are configured; `None` makes
    /// the upload routes 503 with "asset storage not configured".
    pub assets: Option<SharedAssetStore>,
    /// Workspace-settings store. Reads/writes `organizations`,
    /// `org_members`, `org_invites`. See [`crate::orgs::OrgStore`].
    pub orgs: SharedOrgStore,
    /// Outbound mail for workspace invites. The default impl writes
    /// to logs (see [`crate::orgs::LogMailer`]); production builds
    /// swap in an SMTP / SES implementation at app construction.
    pub mailer: SharedMailer,
}

impl AppState {
    /// Accessor for the cookie-Secure flag. Convenience over reading
    /// the public field; keeps the route module readable.
    #[must_use]
    pub fn cookie_secure(&self) -> bool {
        self.cookie_secure
    }
}
