//! End-to-end probe for the sessions-tenancy retrofit.
//!
//! Same three contracts as `tests/auth_mcp_servers.rs` and
//! `tests/auth_agents.rs`:
//!   1. Unauthenticated `GET /threads` → 401.
//!   2. Authenticated `GET /threads` for a fresh principal → 200, `[]`.
//!   3. Cross-org isolation: a thread enqueued under org A is invisible
//!      to a request authenticated as org B and vice versa.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::agents::{
    AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent, SharedAgentStore,
};
use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    IdempotencyKey, NewPromptRequest, PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream,
    SharedDagBudget, SharedLeaseManager, SharedPromptQueue, SharedResponseSink,
    SharedResponseSource, SharedThreadStream,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::types::Prompt;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, join_second_org, seed_principal};
use common::pg::seed_tenant;

struct AuthThreadsHarness {
    state: AppState,
    queue: SharedPromptQueue,
    agents: SharedAgentStore,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl AuthThreadsHarness {
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
        // A fresh principal in its *own* org — distinct from the seeded
        // `_seed` org. Its org has no threads yet, which is the
        // baseline the empty-list assertion needs.
        let primary = seed_principal(pool, &jwt).await;

        let state = AppState {
            queue: queue.clone(),
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: std::sync::Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: std::sync::Arc::new(patom::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
            mcp_oauth_pending: std::sync::Arc::new(patom::mcp::oauth::PgMcpOAuthPendingStore::new(
                pool.clone(),
                clock.clone(),
            )),
            oauth_redirect_base: std::sync::Arc::from("http://localhost:8080"),
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
            memberships: std::sync::Arc::new(patom::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
            entitlements: std::sync::Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            state,
            queue,
            agents,
            primary,
            refresher,
        }
    }

    /// Seed an agent in `org`, then enqueue a human-rooted prompt
    /// against it so a thread (sessions + DAG row) materialises under
    /// that org. The principal-side flow is HTTP `POST /prompts`; this
    /// shortcut goes straight to the queue with the right tenancy
    /// fields so the test can assert isolation without standing up a
    /// worker.
    async fn seed_thread(
        &self,
        org_id: OrgId,
        user_id: patom::auth::UserId,
        agent_name: &str,
        content: &str,
        key: &str,
    ) {
        let record = self
            .agents
            .create(NewAgent {
                org_id,
                name: AgentName::try_from(agent_name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("scoped prompt").expect("prompt"),
                description: AgentDescription::try_from(format!("agent {agent_name}"))
                    .expect("desc"),
                is_default: false,
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("seed agent");
        self.queue
            .enqueue(NewPromptRequest {
                session: None,
                sender: common::pg::human_participant(&self.state.pool, org_id, user_id).await,
                receiver_agent_id: record.id,
                parent_session: None,
                content: Prompt::try_from(content).expect("prompt"),
                idempotency_key: IdempotencyKey::try_from(key).expect("key"),
                org_id,
                created_by_user_id: user_id,
                kind_payload: patom::runtime::RequestKindPayload::Normal {},
            })
            .await
            .expect("enqueue thread");
    }
}

#[sqlx::test]
async fn unauthenticated_get_threads_returns_401(pool: PgPool) {
    let h = AuthThreadsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/threads")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn authenticated_new_user_sees_empty_list(pool: PgPool) {
    let h = AuthThreadsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/threads")
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
    assert_eq!(json, serde_json::json!([]));
}

#[sqlx::test]
async fn cross_org_isolation_filters_to_caller_org(pool: PgPool) {
    let h = AuthThreadsHarness::new(&pool).await;

    // Mint a second principal in a different org and seed one thread
    // under each org.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    h.seed_thread(
        h.primary.org_id,
        h.primary.user_id,
        "alpha",
        "primary prompt",
        "k-primary",
    )
    .await;
    h.seed_thread(
        other.org_id,
        other.user_id,
        "beta",
        "other prompt",
        "k-other",
    )
    .await;

    let app = router(h.state.clone());

    // Primary principal sees only their own org's thread.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/threads")
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
    assert_eq!(rows.len(), 1, "primary sees exactly one thread");
    assert_eq!(
        rows[0]["first_agent"]["name"].as_str(),
        Some("alpha"),
        "primary's thread is the one rooted on `alpha`",
    );

    // The other principal sees only theirs.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/threads")
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
    assert_eq!(rows.len(), 1, "other sees exactly one thread");
    assert_eq!(
        rows[0]["first_agent"]["name"].as_str(),
        Some("beta"),
        "other's thread is the one rooted on `beta`",
    );
}

#[sqlx::test]
async fn list_scoped_to_active_org_not_all_memberships(pool: PgPool) {
    // A user who belongs to two orgs must see only the *active* org's
    // threads in the channel feed. RLS gates on membership in any org, so
    // the feed query has to pin the active org explicitly.
    let h = AuthThreadsHarness::new(&pool).await;

    // Make the primary user a member of a second org as well.
    let other_org = join_second_org(&h.state.pool, h.primary.user_id).await;

    // One thread in the active org, one in the other org the user belongs to.
    h.seed_thread(
        h.primary.org_id,
        h.primary.user_id,
        "active-agent",
        "active prompt",
        "k-active",
    )
    .await;
    h.seed_thread(
        other_org,
        h.primary.user_id,
        "other-agent",
        "other prompt",
        "k-other",
    )
    .await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/threads")
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
        .map(|r| r["first_agent"]["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["active-agent"],
        "only the active org's thread is listed",
    );
}
