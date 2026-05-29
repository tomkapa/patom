//! End-to-end probe for the agents-tenancy retrofit.
//!
//! Verifies the same three contracts as `tests/auth_mcp_servers.rs`:
//!   1. Unauthenticated `GET /agents` → 401.
//!   2. Authenticated `GET /agents` for a fresh principal → 200; the
//!      principal sees the agents in their own org (seeded via the
//!      store) and nothing from other orgs.
//!   3. Cross-org isolation: an agent inserted under org A is invisible
//!      to a request authenticated as org B and vice versa.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom_rs::agents::{
    AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent, SharedAgentStore,
};
use patom_rs::auth::OrgId;
use patom_rs::clock::SystemClock;
use patom_rs::http::{AppState, router};
use patom_rs::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom_rs::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::seed_tenant;

struct AuthAgentsHarness {
    state: AppState,
    agents: SharedAgentStore,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl AuthAgentsHarness {
    async fn new(pool: &PgPool) -> Self {
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
        let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom_rs::mcp::SharedMcpCatalogStore =
            Arc::new(patom_rs::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: patom_rs::memory::SharedMemoryStore =
            Arc::new(patom_rs::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));

        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        // Fresh principal in its *own* org — distinct from the seeded
        // `_seed` org (which already has the `test-default`
        // agent). The primary principal's org has no agents yet, which
        // is the right baseline for the empty-list assertion below.
        let primary = seed_principal(pool, &jwt).await;

        let state = AppState {
            queue,
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            dag,
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: std::sync::Arc::new(patom_rs::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                std::sync::Arc::new(patom_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom_rs::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
            mcp_oauth_pending: std::sync::Arc::new(
                patom_rs::mcp::oauth::PgMcpOAuthPendingStore::new(pool.clone(), clock.clone()),
            ),
            oauth_redirect_base: std::sync::Arc::from("http://localhost:8080"),
            web_base_url: None,
            thread_stream,
            pool: pool.clone(),
            jwt,
            oauth,
            users,
            clock: clock.clone(),
            cookie_secure: false,
            memberships: std::sync::Arc::new(patom_rs::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
            orgs: std::sync::Arc::new(patom_rs::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom_rs::orgs::LogMailer),
        };

        Self {
            state,
            agents,
            primary,
            refresher,
        }
    }

    async fn seed_agent(&self, org: OrgId, name: &str) {
        self.agents
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
                edited_by: None,
            })
            .await
            .expect("seed agent");
    }
}

#[sqlx::test]
async fn unauthenticated_get_agents_returns_401(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn authenticated_new_user_sees_empty_list(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    // The primary principal's org is fresh; the seeded `test-default`
    // belongs to `_seed.org_id`, which the primary is not a member
    // of — RLS filters it out.
    assert_eq!(json, serde_json::json!([]));
}

#[sqlx::test]
async fn cross_org_isolation_filters_to_caller_org(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;

    // Second principal in a different org.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    h.seed_agent(h.primary.org_id, "alpha").await;
    h.seed_agent(other.org_id, "beta").await;

    let app = router(h.state.clone());

    // Primary principal sees only their own org's row.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let names: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["alpha"]);

    // The other principal sees only theirs.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", other.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let names: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["beta"]);
}
