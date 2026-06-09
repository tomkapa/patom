//! End-to-end probe for the entitlement seam (issue #134).
//!
//! Proves the agent-count quota gate wired into `POST /agents`:
//!   1. Under a capped policy, an org at its agent ceiling is refused with
//!      **402 Payment Required** and a body naming the limit.
//!   2. The same capped policy still admits an org below the ceiling (the gate
//!      is a ceiling, not a blanket deny).
//!
//! The shipped OSS default ([`patom::entitlements::UnlimitedEntitlements`])
//! never trips this gate; the cap here comes from a test-local restrictive
//! [`Entitlements`] impl standing in for the future billing-backed cloud impl.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::agents::{AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent};
use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::entitlements::{AgentLimit, Entitlements, Feature, SharedEntitlements};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::seed_tenant;

/// Restrictive policy standing in for the cloud billing-backed impl: caps
/// agents at `max` and licenses no feature. Exercises the deny paths the OSS
/// `UnlimitedEntitlements` default never takes.
#[derive(Debug)]
struct CappedTestEntitlements {
    max: u32,
}

impl Entitlements for CappedTestEntitlements {
    fn agent_limit(&self, _org: OrgId) -> AgentLimit {
        AgentLimit::Max(self.max)
    }
    fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        false
    }
}

struct Harness {
    state: AppState,
    primary: SeededPrincipal,
    #[allow(dead_code)] // keeps the MCP refresh task alive for the request's lifetime.
    refresher: McpRefresher,
}

impl Harness {
    /// Build an HTTP `AppState` whose entitlement policy caps agents at `max`.
    async fn new(pool: &PgPool, max: u32) -> Self {
        let _seed = seed_tenant(pool).await;
        let clock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();
        let leases: SharedLeaseManager = queue_impl;

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let sessions: SharedSessionStore =
            Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
        let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: patom::memory::SharedMemoryStore =
            Arc::new(patom::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));

        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        // Fresh principal in its own (empty) org — the right baseline for the
        // capacity assertions below.
        let primary = seed_principal(pool, &jwt).await;

        let entitlements: SharedEntitlements = Arc::new(CappedTestEntitlements { max });

        let state = AppState {
            queue,
            leases,
            responses,
            sessions,
            agents,
            colleagues: Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                SystemClock::shared(),
            )),
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: Arc::new(patom::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: Arc::new(std::collections::HashMap::new()),
            mcp_oauth_pending: Arc::new(patom::mcp::oauth::PgMcpOAuthPendingStore::new(
                pool.clone(),
                clock.clone(),
            )),
            oauth_redirect_base: Arc::from("http://localhost:8080"),
            web_base_url: None,
            thread_stream,
            pool: pool.clone(),
            jwt,
            oauth,
            bootstrap_admin: false,
            cloud: false,
            users,
            clock: clock.clone(),
            cookie_secure: false,
            cookie_domain: None,
            cors_allowed_origins: Vec::new(),
            memberships: Arc::new(patom::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
            orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: Arc::new(patom::orgs::LogMailer),
            entitlements,
        };

        Self {
            state,
            primary,
            refresher,
        }
    }

    async fn seed_agent(&self, org: OrgId, name: &str) {
        self.state
            .agents
            .create(NewAgent {
                org_id: org,
                name: AgentName::try_from(name).expect("valid name"),
                system_prompt: AgentSystemPrompt::try_from("scoped test prompt")
                    .expect("valid prompt"),
                description: AgentDescription::try_from(format!("agent {name}"))
                    .expect("valid desc"),
                is_default: false,
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("seed agent");
    }

    fn post_agent(&self, name: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("cookie", self.primary.cookie_header())
            .header("x-csrf-token", self.primary.csrf_header())
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": name,
                    "system_prompt": "be helpful",
                    "description": "a gated agent",
                }))
                .unwrap(),
            ))
            .expect("request")
    }
}

#[sqlx::test]
async fn create_agent_at_cap_returns_402(pool: PgPool) {
    // Cap of 1, and the org already holds one agent → the next create is over
    // the ceiling.
    let h = Harness::new(&pool, 1).await;
    h.seed_agent(h.primary.org_id, "first").await;

    let app = router(h.state.clone());
    let res = app.oneshot(h.post_agent("second")).await.expect("response");

    assert_eq!(
        res.status(),
        axum::http::StatusCode::PAYMENT_REQUIRED,
        "an org at its agent cap must be refused with 402",
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let message = json["error"].as_str().expect("error message");
    assert!(
        message.contains("agent limit reached"),
        "402 body should explain the cap, got {message:?}",
    );
}

#[sqlx::test]
async fn create_agent_below_cap_succeeds(pool: PgPool) {
    // Same cap of 1, but the org is empty → the gate admits the first create.
    let h = Harness::new(&pool, 1).await;

    let app = router(h.state.clone());
    let res = app.oneshot(h.post_agent("first")).await.expect("response");

    assert_eq!(
        res.status(),
        axum::http::StatusCode::CREATED,
        "an org below its agent cap must be allowed to create",
    );
}
