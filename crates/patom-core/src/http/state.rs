use std::path::PathBuf;
use std::sync::Arc;

use sqlx::PgPool;

use crate::agents::SharedAgentStore;
use crate::assets::SharedAssetStore;
use crate::auth::{
    CookieDomain, JwtSigner, SharedOidcAuth, SharedOrgLanguageResolver, SharedOrgRuleResolver,
    SharedUserStore,
};
use crate::billing::SharedBillingService;
use crate::clock::SharedClock;
use crate::entitlements::SharedEntitlements;
use crate::http::MembershipCache;
use crate::mcp::oauth::SharedMcpOAuthPendingStore;
use crate::mcp::{
    McpRefreshTrigger, SharedMcpCatalogStore, SharedMcpCredentialStore, SharedMcpServerStore,
    TestConnectRateLimiter,
};
use crate::memory::SharedMemoryStore;
use crate::orgs::{SharedMailer, SharedOrgStore};
use crate::prompts::Prompts;
use crate::runtime::{
    SharedDagBudget, SharedPromptQueue, SharedResponseSource, SharedThreadStream,
};
use crate::slack::SlackAppState;

/// Cheaply-cloneable container of every collaborator the HTTP routes need. The router
/// gets a single `AppState` and threads it through axum's extractors.
#[derive(Clone, Debug)]
pub struct AppState {
    pub queue: SharedPromptQueue,
    pub responses: SharedResponseSource,
    pub agents: SharedAgentStore,
    /// Per-org colleague directory — humans + agents as one addressable
    /// roster. HTTP handlers resolve `(org_id, user_id)` → colleague to
    /// construct colleague-backed `Participant::Human` for prompt enqueues.
    pub colleagues: crate::colleagues::SharedColleagueStore,
    /// DAG turn-budget handle. Threaded through state so `send_message`
    /// can `bump_or_fail` and the worker's quiescence trigger can query
    /// liveness without re-constructing the impl.
    pub dag: SharedDagBudget,
    /// Per-org spend budget. The admission gate (`POST /prompts`) calls
    /// `check_or_fail_for_user` through this handle; the same `Arc` is shared
    /// with the agent worker so HTTP and worker enforce one budget seam.
    pub billing: SharedBillingService,
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
    /// Env-keyed Patom-supported OAuth clients
    /// (`PATOM_<X>_CLIENT_ID/_SECRET`). Read by
    /// [`crate::mcp::oauth::start_authorization`] /
    /// [`crate::mcp::oauth::handle_callback`] for catalog entries marked
    /// `client_source = 'platform'`.
    pub platform_oauth_clients:
        std::sync::Arc<std::collections::HashMap<String, crate::config::PlatformOAuthClient>>,
    /// Pending-authorization rows that bridge `POST /oauth/start` →
    /// `GET /oauth/callback`. Postgres-backed so the callback can land
    /// on any replica.
    pub mcp_oauth_pending: SharedMcpOAuthPendingStore,
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
    /// Shared connection pool. The threads routes build a per-request
    /// `PgThreadStore` from this `pool` + `clock` (a pair of `Arc` clones) to
    /// reach the `ThreadStore` trait (channel feed + flat history); other
    /// routes that still hold raw thread/feed SQL use it directly. This field
    /// is the seam for both.
    pub pool: PgPool,
    /// JWT signer used by the auth middleware to verify cookies and by
    /// the OAuth callback route to mint them.
    pub jwt: JwtSigner,
    /// Login OIDC provider (ADR-0011) — owns the discovered endpoints +
    /// JWKS, the client id/secret, and the redirect URL. Google is one
    /// issuer behind this seam. `dyn` so tests inject a fake without
    /// discovery or a live IdP.
    pub oauth: SharedOidcAuth,
    /// First-admin bootstrap toggle (`PATOM_BOOTSTRAP_ADMIN`). When true
    /// and the org table is empty, the OAuth callback routes the very
    /// first login through the audited, count-guarded bootstrap path.
    pub bootstrap_admin: bool,
    /// Cloud (SaaS) build flag, derived from `cfg!(feature = "cloud")` at
    /// the composition root — a BUILD-MODE signal, not a runtime env var.
    /// Gates self-service workspace creation (`POST /me/orgs`) and the
    /// org-less onboarding flow (the OAuth callback mints an org-less
    /// session instead of auto-creating a personal org). `false` on the
    /// OSS / self-host build, which keeps today's auto-create-at-login
    /// behavior. Orthogonal to [`Self::bootstrap_admin`].
    pub cloud: bool,
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
    /// Shared cookie `Domain` for the session + CSRF cookies. `Some`
    /// (e.g. `.patom.app`) shares them across the apex marketing site and
    /// the `app.` subdomain; `None` keeps them host-only (localhost dev).
    /// Sourced from [`crate::config::AuthSettings`].
    pub cookie_domain: Option<CookieDomain>,
    /// Cross-origin allowlist for the `/api` subtree (e.g.
    /// `https://patom.app`). Empty disables the CORS layer. Sourced from
    /// [`crate::config::AuthSettings`].
    pub cors_allowed_origins: Vec<String>,
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
    /// `index.html` with `window.__PATOM_CONFIG__` injected before `</head>`.
    /// Built once at startup from `PATOM_POSTHOG_KEY` / `PATOM_POSTHOG_HOST`.
    /// Empty string when `web_dist/index.html` does not exist (dev without a
    /// built SPA). Served by the not-found fallback so React Router deep links
    /// get the correct config without a network roundtrip.
    pub index_html: Arc<str>,
    /// Slack adapter wiring — `Some` when `PATOM_SLACK_*` env vars are
    /// configured, `None` otherwise. Public webhook + OAuth handlers
    /// 404 cleanly when this is `None`, so deployments without Slack
    /// stay first-class.
    pub slack: Option<SlackAppState>,
    /// Object-storage seam for user avatars and MCP catalog icons.
    /// `Some` when `PATOM_S3_*` env vars are configured; `None` makes
    /// the upload routes 503 with "asset storage not configured".
    pub assets: Option<SharedAssetStore>,
    /// Workspace-settings store. Reads/writes `organizations`,
    /// `org_members`, `org_invites`. See [`crate::orgs::OrgStore`].
    pub orgs: SharedOrgStore,
    /// Outbound mail for workspace invites. The default impl writes
    /// to logs (see [`crate::orgs::LogMailer`]); production builds
    /// swap in an SMTP / SES implementation at app construction.
    pub mailer: SharedMailer,
    /// Entitlement policy seam (issue #134). The OSS / self-host build
    /// runs [`crate::entitlements::UnlimitedEntitlements`] (unlimited
    /// agents, every feature on); `patom-cloud` overrides this under
    /// `--features cloud` with a billing-backed impl that resolves an
    /// org's paid tier and agent cap. The `POST /agents` handler gates
    /// creation through it.
    pub entitlements: SharedEntitlements,
}

impl AppState {
    /// Accessor for the cookie-Secure flag. Convenience over reading
    /// the public field; keeps the route module readable.
    #[must_use]
    pub fn cookie_secure(&self) -> bool {
        self.cookie_secure
    }

    /// Accessor for the shared cookie `Domain`. `None` when unset, which
    /// the cookie builders translate to "omit the `Domain` attribute".
    #[must_use]
    pub fn cookie_domain(&self) -> Option<&CookieDomain> {
        self.cookie_domain.as_ref()
    }
}
