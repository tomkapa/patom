//! Integration tests for `POST /prompts`.
//!
//! Specifically guards the receiver-resolution rule: when the request
//! carries a `session_id`, the receiver agent must be derived from the
//! session's existing agent participant — never from the caller-supplied
//! `agent_id` and never from the seeded default. The previous behavior
//! defaulted to `agents.default_id()`, which produced
//! "agent X is not a participant of session Y" once the worker tried to
//! load history with the wrong viewer (the bug that motivated this test).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use relay_rs::agents::{AgentName, AgentSystemPrompt, NewAgent, SharedAgentStore};
use relay_rs::clock::{SharedClock, SystemClock};
use relay_rs::http::{AppState, router};
use relay_rs::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use relay_rs::runtime::{
    IdempotencyKey, NewPromptRequest, PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream,
    SharedDagBudget, SharedLeaseManager, SharedPromptQueue, SharedResponseSink,
    SharedResponseSource, SharedThreadStream,
};
use relay_rs::session::{PgSessionStore, SharedSessionStore};
use relay_rs::types::{Participant, Prompt};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::pg::TestDb;

struct PromptsHarness {
    db: TestDb,
    queue: SharedPromptQueue,
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
    async fn new() -> Self {
        let db = TestDb::fresh().await;
        let clock: SharedClock = SystemClock::shared();
        let pool: PgPool = db.pool.clone();

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
        let mcp_catalog: relay_rs::mcp::SharedMcpCatalogStore =
            Arc::new(relay_rs::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: relay_rs::memory::SharedMemoryStore =
            Arc::new(relay_rs::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));
        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        // Pin the principal to the same org as `db.default_agent_id` so
        // the `default_id_for(principal.active_org_id)` fallback in the
        // route resolves to the seeded default. The cross-org isolation
        // path is exercised in `tests/auth_agents.rs`.
        let seeded =
            common::auth::principal_for_default_org(db.default_user_id, db.default_org_id, &jwt);
        let state = AppState {
            queue: queue.clone(),
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            dag,
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: std::sync::Arc::new(relay_rs::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                std::sync::Arc::new(relay_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: relay_rs::mcp::TestConnectRateLimiter::new(clock.clone()),
            mcp_oauth_clients: std::sync::Arc::new(
                relay_rs::mcp::oauth::PgMcpOAuthClientStore::new(
                    pool.clone(),
                    clock.clone(),
                    std::sync::Arc::new(relay_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
                ),
            ),
            mcp_oauth_pending: std::sync::Arc::new(
                relay_rs::mcp::oauth::PgMcpOAuthPendingStore::new(pool.clone(), clock.clone()),
            ),
            mcp_oauth_flow: relay_rs::mcp::oauth::OAuthFlowClient::new(reqwest::Client::new())
                .expect("oauth http"),
            oauth_redirect_base: std::sync::Arc::from("http://localhost:8080"),
            web_base_url: None,
            thread_stream,
            pool,
            jwt,
            oauth,
            users,
            clock: clock.clone(),
            cookie_secure: false,
            memberships: std::sync::Arc::new(relay_rs::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
        };

        Self {
            db,
            queue,
            agents,
            state,
            auth_cookie: seeded.cookie_header(),
            refresher,
        }
    }

    async fn create_agent(&self, name: &str) -> relay_rs::agents::AgentId {
        self.agents
            .create(NewAgent {
                org_id: self.db.default_org_id,
                name: AgentName::try_from(name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("test prompt").expect("prompt"),
                description: relay_rs::agents::AgentDescription::try_from("test agent")
                    .expect("desc"),
                is_default: false,
                allowed_mcp_tools: relay_rs::agents::AllowedMcpTools::empty(),
                model: None,
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
                    relay_rs::auth::limits::CSRF_HEADER_NAME,
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

/// Regression: posting a follow-up to an existing session must enqueue
/// against the session's agent participant, not the system default.
/// Previously the route blindly fell back to `agents.default_id()`, the
/// worker rejected the prompt with
/// `agent X is not a participant of session Y`, and the run errored out.
#[tokio::test(flavor = "multi_thread")]
async fn followup_with_session_id_routes_to_session_agent() {
    let h = PromptsHarness::new().await;

    // The session is bound to a non-default agent — exactly the DM scenario
    // that exposed the bug ("when I reply thread in thread detail in direct
    // message"). The default agent is the seeded one; `translator` is what
    // the user is talking to.
    let translator = h.create_agent("translator").await;

    // Open the session with a Human → translator root prompt.
    let root = h
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: Participant::Human,
            receiver_agent_id: translator,
            parent_session: None,
            content: Prompt::try_from("hi").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k-root").expect("key"),
            org_id: h.db.default_org_id,
            created_by_user_id: h.db.default_user_id,
            kind_payload: relay_rs::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue root");
    let session_id = root.session();

    // Follow-up: no `agent_id`. Pre-fix, this defaulted to the seeded
    // `test-default` agent — wrong receiver, wrong-participant error.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "session_id": session_id.as_uuid(),
            "content": "follow-up",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let request_id_str = body["request_id"].as_str().expect("request_id");

    // Confirm the persisted row has receiver = translator, NOT the default.
    let row: (Uuid,) =
        sqlx::query_as("SELECT receiver_agent_id FROM prompt_requests WHERE id = $1::uuid")
            .bind(request_id_str)
            .fetch_one(&h.db.pool)
            .await
            .expect("fetch new row");
    assert_eq!(
        row.0,
        translator.as_uuid(),
        "follow-up should route to the session's translator, not the seeded default {}",
        h.db.default_agent_id.as_uuid(),
    );

    // Sanity: a follow-up that *also* names a different agent_id is still
    // routed to the session's participant — the route must not honour a
    // mismatched override.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "session_id": session_id.as_uuid(),
            "agent_id": h.db.default_agent_id.as_uuid(),
            "content": "follow-up 2",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let request_id_str = body["request_id"].as_str().expect("request_id");
    let row: (Uuid,) =
        sqlx::query_as("SELECT receiver_agent_id FROM prompt_requests WHERE id = $1::uuid")
            .bind(request_id_str)
            .fetch_one(&h.db.pool)
            .await
            .expect("fetch new row");
    assert_eq!(
        row.0,
        translator.as_uuid(),
        "client-supplied agent_id must be ignored when session_id is set",
    );
}

/// New session with no `agent_id` falls back to the seeded default — the
/// only path the legacy behavior is still correct on.
#[tokio::test(flavor = "multi_thread")]
async fn new_session_without_agent_id_uses_default() {
    let h = PromptsHarness::new().await;

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
        sqlx::query_as("SELECT receiver_agent_id FROM prompt_requests WHERE id = $1::uuid")
            .bind(request_id_str)
            .fetch_one(&h.db.pool)
            .await
            .expect("fetch new row");
    assert_eq!(row.0, h.db.default_agent_id.as_uuid());
}
