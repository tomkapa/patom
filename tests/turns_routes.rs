//! Integration tests for `GET /turns/:request_id` (the Logs & Metrics
//! drawer endpoint). Driven through the live axum router and a real
//! Postgres schema — the queries this endpoint runs use the string-based
//! `sqlx::query_as` API, so column drift cannot be caught at compile
//! time. Two recent regressions slipped past unit tests:
//!
//! 1. `fetch_reasoning` referenced the `session_messages.role` column
//!    after migration 4 dropped it (replaced by `sender_kind`).
//! 2. `fetch_tool_calls` referenced `mcp_servers.alias` after
//!    migration 31 dropped it (replaced by `catalog_id`).
//!
//! Each shipped as a 500 the first time the FE clicked a row in the
//! per-turn audit. Anchor each query against the live schema here so the
//! next column rename surfaces as a red test, not a runtime 500.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use chrono::Utc;
use patom::agent_core::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, PgTurnMetricsStore, StopReasonLabel, TurnMetricsRow,
    TurnMetricsStore,
};
use patom::agents::prompt_versions::PromptVersionId;
use patom::agents::{AgentId, SharedAgentStore};
use patom::auth::{OrgId, UserId};
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{
    ConnectionStatus, McpCatalogId, McpHttpUrl, McpRefresher, McpRegistry, McpServerCreate,
    McpServerId, McpTransport, PgMcpServerStore, SharedMcpServerStore,
};
use patom::provider::{AssistantContent, ChatMessage};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, PromptRequestId, SharedDagBudget,
    SharedLeaseManager, SharedPromptQueue, SharedResponseSink, SharedResponseSource,
    SharedThreadStream,
};
use patom::session::{PgSessionStore, SessionId, SharedSessionStore};
use patom::tools::ToolCallRowId;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, principal_for_default_org, seed_principal};
use common::pg::{Seed, human_to_agent_session, seed_prompt_request, seed_tenant};

// ─── harness ───────────────────────────────────────────────────────────

struct Harness {
    seed: Seed,
    state: AppState,
    primary: SeededPrincipal,
    mcp_store: SharedMcpServerStore,
    #[allow(dead_code)]
    agents: SharedAgentStore,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl Harness {
    async fn new(pool: PgPool) -> Self {
        let seed = seed_tenant(&pool).await;
        let clock: SharedClock = SystemClock::shared();

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
        let primary = principal_for_default_org(seed.user_id, seed.org_id, &jwt);
        let state = AppState {
            queue,
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            dag,
            budget: std::sync::Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: Arc::new(patom::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
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
        };

        Self {
            seed,
            state,
            primary,
            mcp_store,
            agents,
            refresher,
        }
    }

    async fn seed_mcp(&self, org: OrgId, created_by: UserId, catalog_id: &str) -> McpServerId {
        sqlx::query(
            "INSERT INTO mcp_catalog \
                (id, org_id, display_name, description, default_transport, auth_kind) \
             VALUES ($1, NULL, $1, $1, '{\"type\":\"http\",\"url\":\"https://example.com/mcp\"}'::jsonb, 'none') \
             ON CONFLICT DO NOTHING",
        )
        .bind(catalog_id)
        .execute(&self.state.pool)
        .await
        .expect("seed mcp_catalog");
        let catalog_id = McpCatalogId::try_from(catalog_id).expect("valid catalog id");
        self.mcp_store
            .create(McpServerCreate {
                org_id: org,
                created_by_user_id: created_by,
                catalog_id,
                config: McpTransport::Http {
                    url: McpHttpUrl::try_from("http://localhost:9000/probe").expect("valid url"),
                },
                description: None,
                enabled: true,
                connection_status: ConnectionStatus::Ok,
            })
            .await
            .expect("seed mcp server")
            .id
    }
}

// ─── seed helpers ──────────────────────────────────────────────────────

/// Pick the prompt-version id seeded by migration 43 for `agent_id`.
async fn current_prompt_version(pool: &PgPool, agent_id: AgentId) -> PromptVersionId {
    sqlx::query_scalar::<_, PromptVersionId>(
        "SELECT id FROM agent_prompt_versions \
         WHERE agent_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
    .expect("seeded prompt version")
}

async fn record_turn_metrics(
    pool: &PgPool,
    org: OrgId,
    session: SessionId,
    request: PromptRequestId,
    agent: AgentId,
    pvid: PromptVersionId,
) {
    let store = PgTurnMetricsStore::new(pool.clone(), SystemClock::shared());
    let row = TurnMetricsRow {
        request_id: request,
        org_id: org,
        session_id: session,
        agent_id: agent,
        prompt_version_id: pvid,
        kind: patom::runtime::RequestKind::Normal,
        model: patom::provider::Model::try_from("test-model").expect("catalog"),
        provider: patom::provider::ProviderId::Anthropic,
        input_tokens: InputTokens::try_from(100u32).expect("fits"),
        output_tokens: OutputTokens::try_from(42u32).expect("fits"),
        cache_creation_tokens: None,
        cache_read_tokens: Some(InputTokens::try_from(7u32).expect("fits")),
        duration_ms: DurationMs::saturating_from_millis(1234),
        stop_reason: StopReasonLabel::from_truncated("tool_use"),
        started_at: Utc::now(),
    };
    store.record(row).await.expect("record turn metrics");
}

struct ToolCallSeed<'a> {
    pool: &'a PgPool,
    org: OrgId,
    session: SessionId,
    request: PromptRequestId,
    agent: AgentId,
    mcp_server: Option<McpServerId>,
    tool_name: &'a str,
    is_error: bool,
}

async fn insert_tool_call(seed: ToolCallSeed<'_>) {
    let ToolCallSeed {
        pool,
        org,
        session,
        request,
        agent,
        mcp_server,
        tool_name,
        is_error,
    } = seed;
    sqlx::query(
        "INSERT INTO tool_calls
             (id, org_id, session_id, request_id, agent_id,
              mcp_server_id, tool_name, started_at, duration_ms,
              is_error, error_message, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $8)",
    )
    .bind(ToolCallRowId::new())
    .bind(org)
    .bind(session)
    .bind(request)
    .bind(agent)
    .bind(mcp_server)
    .bind(tool_name)
    .bind(Utc::now())
    .bind(8_i32)
    .bind(is_error)
    .bind(if is_error { Some("boom") } else { None })
    .execute(pool)
    .await
    .expect("insert tool_call");
}

async fn http_get(
    state: AppState,
    uri: &str,
    cookie: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let app = router(state);
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header("cookie", cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    };
    (status, json)
}

// ─── tests ─────────────────────────────────────────────────────────────

/// End-to-end: every fetch helper inside the route runs against the live
/// schema. Asserts the column-bound joins (`session_messages.sender_kind`,
/// `mcp_servers.catalog_id`, `agent_prompt_versions`) all resolve and the
/// response carries the seeded data.
#[sqlx::test]
async fn fetches_turn_detail_with_reasoning_and_tool_calls(pool: PgPool) {
    let h = Harness::new(pool).await;
    let server = h.seed_mcp(h.seed.org_id, h.seed.user_id, "notion").await;

    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let pvid = current_prompt_version(&h.state.pool, h.seed.agent_id).await;
    record_turn_metrics(
        &h.state.pool,
        h.seed.org_id,
        session,
        request,
        h.seed.agent_id,
        pvid,
    )
    .await;

    // session_messages row with an Assistant body carrying a reasoning
    // block — fetch_reasoning must pick this up via `sender_kind = 'agent'`.
    h.state
        .sessions
        .append(
            session,
            patom::types::MessageSender::Agent {
                agent_id: h.seed.agent_id,
            },
            patom::types::Participant::Human,
            ChatMessage::Assistant(vec![
                AssistantContent::Reasoning("thinking step 1".into()),
                AssistantContent::Text("hello".into()),
                AssistantContent::Reasoning("thinking step 2".into()),
            ]),
            request,
        )
        .await
        .expect("append assistant message");

    insert_tool_call(ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: h.seed.agent_id,
        mcp_server: Some(server),
        tool_name: "pages.search",
        is_error: false,
    })
    .await;

    let uri = format!("/api/turns/{}", request.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");

    // Turn metrics round-trip.
    assert_eq!(body["turn"]["input_tokens"], 100);
    assert_eq!(body["turn"]["output_tokens"], 42);
    assert_eq!(body["turn"]["cache_read_tokens"], 7);
    assert_eq!(body["turn"]["stop_reason"], "tool_use");
    assert_eq!(body["turn"]["model"], "test-model");

    // Reasoning blocks ordered as appended — the SQL filter on
    // `sender_kind = 'agent'` must match the appended row.
    let reasoning = body["reasoning_blocks"].as_array().expect("array");
    assert_eq!(reasoning.len(), 2, "two reasoning blocks expected");
    assert_eq!(reasoning[0]["text"], "thinking step 1");
    assert_eq!(reasoning[1]["text"], "thinking step 2");

    // Tool call projection joins `mcp_servers s ON s.id = tc.mcp_server_id`
    // and pulls `s.catalog_id` — the join column that broke before.
    let tool_calls = body["tool_calls"].as_array().expect("array");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["tool_name"], "pages.search");
    assert_eq!(tool_calls[0]["mcp_server_catalog_id"], "notion");
    assert_eq!(
        tool_calls[0]["mcp_server_id"].as_str(),
        Some(server.as_uuid().to_string().as_str())
    );

    // Prompt version snapshot loaded from `agent_prompt_versions`.
    assert_eq!(body["prompt_version"]["version"], 1);
    assert!(body["prompt_version"]["system_prompt"].is_string());
}

/// Bare turn: only the metrics row exists, no session_messages /
/// tool_calls / memory_events. Every helper query must still execute
/// (an `fetch_all` against an empty result is still a query) and return
/// empty collections — not a 500.
#[sqlx::test]
async fn fetches_turn_detail_with_empty_collections(pool: PgPool) {
    let h = Harness::new(pool).await;

    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let pvid = current_prompt_version(&h.state.pool, h.seed.agent_id).await;
    record_turn_metrics(
        &h.state.pool,
        h.seed.org_id,
        session,
        request,
        h.seed.agent_id,
        pvid,
    )
    .await;

    let uri = format!("/api/turns/{}", request.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");

    assert_eq!(body["reasoning_blocks"].as_array().expect("array").len(), 0);
    assert_eq!(body["tool_calls"].as_array().expect("array").len(), 0);
    assert_eq!(body["memory_writes"].as_array().expect("array").len(), 0);
}

/// Tool call with a null `mcp_server_id` (system tool — `send_message`,
/// `search_agents`, …). The LEFT JOIN must keep the row and surface a
/// null `mcp_server_catalog_id` instead of dropping it.
#[sqlx::test]
async fn keeps_tool_calls_without_mcp_server(pool: PgPool) {
    let h = Harness::new(pool).await;
    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;
    let pvid = current_prompt_version(&h.state.pool, h.seed.agent_id).await;
    record_turn_metrics(
        &h.state.pool,
        h.seed.org_id,
        session,
        request,
        h.seed.agent_id,
        pvid,
    )
    .await;

    insert_tool_call(ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: h.seed.agent_id,
        mcp_server: None,
        tool_name: "send_message",
        is_error: false,
    })
    .await;

    let uri = format!("/api/turns/{}", request.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let tool_calls = body["tool_calls"].as_array().expect("array");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["tool_name"], "send_message");
    assert_eq!(tool_calls[0]["mcp_server_id"], serde_json::Value::Null);
    assert_eq!(
        tool_calls[0]["mcp_server_catalog_id"],
        serde_json::Value::Null
    );
}

/// Unknown request_id ⇒ 404 (via `visible_to` pre-gate), not 500. Guards
/// against the route accidentally re-routing a "no metrics row" path
/// through the 5xx auth bucket as it did before the Db variant split.
#[sqlx::test]
async fn returns_404_for_unknown_request(pool: PgPool) {
    let h = Harness::new(pool).await;
    let stranger = PromptRequestId::new();

    let uri = format!("/api/turns/{}", stranger.as_uuid());
    let (status, _) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

/// Cross-org request_id ⇒ 404 (existence not leaked across orgs). The
/// `visible_to` pre-gate runs before the inner tx, so RLS isolation
/// can't be checked by sneaking a foreign id into the URL.
#[sqlx::test]
async fn returns_404_for_cross_org_request(pool: PgPool) {
    let h = Harness::new(pool).await;
    let foreign = seed_principal(&h.state.pool, &h.state.jwt).await;

    // Seed a request in the *foreign* org, then ask for it with the
    // primary principal's cookie.
    let foreign_agent = h
        .agents
        .create(patom::agents::NewAgent {
            org_id: foreign.org_id,
            name: patom::agents::AgentName::try_from("Eve").expect("name"),
            system_prompt: patom::agents::AgentSystemPrompt::try_from("you are eve")
                .expect("prompt"),
            description: patom::agents::AgentDescription::try_from("foreign agent").expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom::agents::AllowedMcpTools::default(),
            model: None,
            edited_by: None,
        })
        .await
        .expect("create foreign agent")
        .id;
    let foreign_session = human_to_agent_session(
        h.state.sessions.as_ref(),
        foreign_agent,
        foreign.org_id,
        foreign.user_id,
    )
    .await;
    let foreign_request = seed_prompt_request(
        &h.state.pool,
        foreign_session,
        foreign_agent,
        foreign.org_id,
    )
    .await;

    let uri = format!("/api/turns/{}", foreign_request.as_uuid());
    let (status, _) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

/// Unauthenticated request ⇒ 401 from the auth layer, never reaches the
/// route — surfaces if the route is accidentally moved outside the
/// `require_principal` middleware.
#[sqlx::test]
async fn returns_401_without_session(pool: PgPool) {
    let h = Harness::new(pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/turns/00000000-0000-0000-0000-000000000000")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}
