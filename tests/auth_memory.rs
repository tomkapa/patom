//! End-to-end probe for the memory-tenancy retrofit (migration 17).
//!
//! Same three contracts as the prior probes (`tests/auth_mcp_servers.rs`,
//! `tests/auth_agents.rs`, `tests/auth_threads.rs`):
//!   1. Unauthenticated `GET /agents/{id}/memory` → 401.
//!   2. Authenticated request for a fresh principal hitting an agent
//!      they don't own → 404 (RLS hides the agent; the gate translates
//!      that to NotFound).
//!   3. Cross-org isolation: a memory written under agent A in org A is
//!      invisible to a request authenticated as org B, and the seeded
//!      operator note under each org's own agent is the only row each
//!      caller sees.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom_rs::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent,
    SharedAgentStore,
};
use patom_rs::auth::{OrgId, UserId};
use patom_rs::clock::SystemClock;
use patom_rs::http::{AppState, router};
use patom_rs::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom_rs::memory::{
    MemoryContent, MemoryKind, MemoryMutation, MemoryState, MutationSource, PgMemoryStore,
    SharedMemoryStore,
};
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

struct AuthMemoryHarness {
    state: AppState,
    agents: SharedAgentStore,
    memory_store: SharedMemoryStore,
    primary: SeededPrincipal,
    seed_agent_id: patom_rs::agents::AgentId,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl AuthMemoryHarness {
    async fn new(pool: &PgPool) -> Self {
        let seed = seed_tenant(pool).await;
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

        let memory_store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
            pool.clone(),
            clock.clone(),
            common::embedding::FakeEmbeddingProvider::shared(),
        ));

        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        // Fresh principal in its own org — disjoint from
        // `seed.org_id` (whose seeded default agent is the
        // cross-org bait for the isolation test).
        let primary = seed_principal(pool, &jwt).await;

        let state = AppState {
            queue,
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            dag,
            memory_store: memory_store.clone(),
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
            memory_store,
            primary,
            seed_agent_id: seed.agent_id,
            refresher,
        }
    }

    /// Seed a fresh agent under `org_id` and write one operator-note
    /// memory row against it. Returns the agent's id so callers can
    /// hit `/agents/{id}/memory` against it.
    async fn seed_agent_with_memory(
        &self,
        org_id: OrgId,
        _user_id: UserId,
        name: &str,
        body: &str,
    ) -> AgentId {
        let agent = self
            .agents
            .create(NewAgent {
                org_id,
                name: AgentName::try_from(name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("scoped prompt").expect("prompt"),
                description: AgentDescription::try_from(format!("agent {name}")).expect("desc"),
                is_default: false,
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                edited_by: None,
            })
            .await
            .expect("create agent");

        self.memory_store
            .apply(MemoryMutation::Write {
                agent: agent.id,
                kind: MemoryKind::Identity,
                content: MemoryContent::try_from(body).expect("content"),
                state: MemoryState::Held,
                pinned: false,
                source: MutationSource::Operator,
            })
            .await
            .expect("write memory");

        agent.id
    }
}

#[sqlx::test]
async fn unauthenticated_get_memory_returns_401(pool: PgPool) {
    let h = AuthMemoryHarness::new(&pool).await;
    let app = router(h.state.clone());
    // The path-scoped agent id needn't exist — the auth layer rejects
    // before the handler runs.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{}/memory", h.seed_agent_id))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn authenticated_caller_cannot_see_cross_org_agents_memory(pool: PgPool) {
    let h = AuthMemoryHarness::new(&pool).await;
    // `seed_agent_id` belongs to `seed.org_id` — the
    // primary principal is not a member, so the agent gate 404s.
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{}/memory", h.seed_agent_id))
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn cross_org_isolation_filters_to_caller_org(pool: PgPool) {
    let h = AuthMemoryHarness::new(&pool).await;

    // A second principal in a different org. Each org owns one agent
    // with one memory row.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    let agent_primary = h
        .seed_agent_with_memory(h.primary.org_id, h.primary.user_id, "alpha", "primary note")
        .await;
    let agent_other = h
        .seed_agent_with_memory(other.org_id, other.user_id, "beta", "other note")
        .await;

    let app = router(h.state.clone());

    // Primary sees their own memory and can read it.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{agent_primary}/memory"))
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
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1, "primary sees exactly one memory row");
    assert_eq!(rows[0]["content"].as_str(), Some("primary note"));

    // Primary asking about other's agent: 404 — RLS hides the agent so
    // the gate produces a clean not-found response. Crucially we never
    // leak any of the other org's memory content.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{agent_other}/memory"))
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);

    // And the symmetric direction — `other` only sees their row.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{agent_other}/memory"))
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
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1, "other sees exactly one memory row");
    assert_eq!(rows[0]["content"].as_str(), Some("other note"));
}
