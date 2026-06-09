//! Integration tests for `POST /prompts` in the thread-feed model.
//!
//! `POST /prompts` is the @tag entry point: it creates a thread (or continues
//! one), appends the human's posted row addressing the agent, and enqueues a
//! trigger. Receiver resolution: an explicit `agent_id` wins; a continuation
//! without one routes to the thread's current agent (DM continuity); a fresh
//! root falls back to the seeded default.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::agents::{AgentName, AgentSystemPrompt, NewAgent, SharedAgentStore};
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::pg::{Seed, seed_tenant};

struct PromptsHarness {
    seed: Seed,
    pool: PgPool,
    agents: SharedAgentStore,
    state: AppState,
    /// `Cookie:` header value carrying a valid JWT for the seeded test
    /// principal. Threaded into every request these tests issue so the
    /// auth layer admits them.
    auth_cookie: String,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl PromptsHarness {
    async fn new(pool: PgPool) -> Self {
        let seed = seed_tenant(&pool).await;
        let clock: SharedClock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

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
        // Pin the principal to the same org as `seed.agent_id` so
        // the `default_id_for(principal.active_org_id)` fallback in the
        // route resolves to the seeded default. The cross-org isolation
        // path is exercised in `tests/auth_agents.rs`.
        let seeded = common::auth::principal_for_default_org(seed.user_id, seed.org_id, &jwt);
        let state = AppState {
            queue: queue.clone(),
            responses,
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
            seed,
            pool,
            agents,
            state,
            auth_cookie: seeded.cookie_header(),
            refresher,
        }
    }

    async fn create_agent(&self, name: &str) -> patom::agents::AgentId {
        self.agents
            .create(NewAgent {
                org_id: self.seed.org_id,
                name: AgentName::try_from(name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("test prompt").expect("prompt"),
                description: patom::agents::AgentDescription::try_from("test agent").expect("desc"),
                is_default: false,
                allowed_mcp_tools: patom::agents::AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("create agent")
            .id
    }
}

async fn post_json(
    state: AppState,
    uri: &str,
    body: serde_json::Value,
    cookie: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let app = router(state);
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .header(
                    patom::auth::limits::CSRF_HEADER_NAME,
                    common::auth::TEST_CSRF_TOKEN,
                )
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    (status, json)
}

/// Receiver resolution across a root + continuation. An explicit `agent_id`
/// is honored on a root; a continuation in the same thread with no `agent_id`
/// stays with that agent (DM continuity, via `last_agent`).
#[sqlx::test]
async fn root_honors_agent_id_then_continuation_keeps_it(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;

    // `translator` is a non-default agent the user @tags in a fresh DM.
    let translator = h.create_agent("translator").await;

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "agent_id": translator.as_uuid(),
            "content": "hi translator",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let thread_id = body["thread_id"].as_str().expect("thread_id").to_owned();
    assert_receiver_agent(
        &h.pool,
        body["request_id"].as_str().expect("rid"),
        translator,
    )
    .await;

    // Continuation in the SAME thread, no `agent_id` → stays with translator
    // (the thread's current agent), not the seeded default.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "thread_id": thread_id,
            "content": "follow-up",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        body["thread_id"].as_str(),
        Some(thread_id.as_str()),
        "continuation reuses the thread",
    );
    assert_receiver_agent(
        &h.pool,
        body["request_id"].as_str().expect("rid"),
        translator,
    )
    .await;
}

/// Confirm the persisted trigger's receiver colleague resolves to `agent`.
async fn assert_receiver_agent(pool: &PgPool, request_id: &str, agent: patom::agents::AgentId) {
    let row: (Uuid,) = sqlx::query_as(
        "SELECT rc.agent_id FROM prompt_requests pr \
         JOIN colleagues rc ON rc.id = pr.receiver_colleague_id WHERE pr.id = $1::uuid",
    )
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("fetch row");
    assert_eq!(
        row.0,
        agent.as_uuid(),
        "trigger routed to the expected agent"
    );
}

/// New root with no `agent_id` falls back to the seeded default agent.
#[sqlx::test]
async fn new_root_without_agent_id_uses_default(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "content": "first hello",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let request_id_str = body["request_id"].as_str().expect("request_id");

    let row: (Uuid,) =
        sqlx::query_as("SELECT rc.agent_id FROM prompt_requests pr JOIN colleagues rc ON rc.id = pr.receiver_colleague_id WHERE pr.id = $1::uuid")
            .bind(request_id_str)
            .fetch_one(&h.pool)
            .await
            .expect("fetch new row");
    assert_eq!(row.0, h.seed.agent_id.as_uuid());
}

/// Admission gate: an org that has spent its monthly cap gets a 429 on
/// `POST /prompts` before any work is enqueued.
#[sqlx::test]
async fn over_budget_org_is_rejected_with_429(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;

    // Cap the org at 1 micro-USD and record 1000 already spent this period.
    common::pg::set_budget(&h.pool, h.seed.org_id, Some(1), 8000).await;
    common::pg::seed_period_usage(&h.pool, h.seed.org_id, 1000).await;

    let (status, _body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "content": "should be rejected",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);

    // Nothing was enqueued — the gate ran before the queue insert.
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM prompt_requests WHERE org_id = $1")
        .bind(h.seed.org_id)
        .fetch_one(&h.pool)
        .await
        .expect("count requests");
    assert_eq!(count, 0, "over-budget prompt must not enqueue work");
}
