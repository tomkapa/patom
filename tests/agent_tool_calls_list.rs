//! Integration tests for `GET /agents/{id}/tool-calls`, driven
//! through the live axum router so the auth middleware + RLS path are
//! part of the test surface. Mirrors `mcp_tool_calls_list.rs` but pivots
//! the audit list on the agent dimension and projects the originating
//! MCP server (id + alias) per row so the FE can render the connection
//! chip without a second fetch.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use patom::agents::{AgentId, SharedAgentStore};
use patom::auth::{OrgId, UserId};
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{
    ConnectionStatus, McpCatalogId, McpHttpUrl, McpRefresher, McpRegistry, McpServerCreate,
    McpServerId, McpTransport, PgMcpServerStore, SharedMcpServerStore,
};
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
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};

struct Harness {
    seed: common::pg::Seed,
    state: AppState,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    agents: SharedAgentStore,
    mcp_store: SharedMcpServerStore,
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
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
        };

        Self {
            seed,
            state,
            primary,
            agents,
            mcp_store,
            refresher,
        }
    }

    async fn seed_mcp(&self, org: OrgId, created_by: UserId, catalog_id: &str) -> McpServerId {
        // FK trigger requires a matching `mcp_catalog` row. Tests use
        // bespoke ids; seed each one as a global default before create.
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
        let config = McpTransport::Http {
            url: McpHttpUrl::try_from("http://localhost:9000/probe").expect("valid url"),
        };
        self.mcp_store
            .create(McpServerCreate {
                org_id: org,
                created_by_user_id: created_by,
                catalog_id,
                config,
                description: None,
                enabled: true,
                connection_status: ConnectionStatus::Ok,
            })
            .await
            .expect("seed mcp server")
            .id
    }
}

struct ToolCallSeed<'a> {
    pool: &'a PgPool,
    org: OrgId,
    session: SessionId,
    request: PromptRequestId,
    agent: AgentId,
    mcp_server: Option<McpServerId>,
    tool_name: &'a str,
    started_at: DateTime<Utc>,
    is_error: bool,
    error_message: Option<&'a str>,
}

async fn insert_tool_call(seed: ToolCallSeed<'_>) {
    let id = ToolCallRowId::new();
    sqlx::query(
        "INSERT INTO tool_calls
             (id, org_id, session_id, request_id, agent_id,
              mcp_server_id, tool_name, started_at, duration_ms,
              is_error, error_message, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $8)",
    )
    .bind(id)
    .bind(seed.org)
    .bind(seed.session)
    .bind(seed.request)
    .bind(seed.agent)
    .bind(seed.mcp_server)
    .bind(seed.tool_name)
    .bind(seed.started_at)
    .bind(7_i32)
    .bind(seed.is_error)
    .bind(seed.error_message)
    .execute(seed.pool)
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

#[sqlx::test]
async fn lists_tool_calls_for_agent_across_connections_with_server_alias(pool: PgPool) {
    let h = Harness::new(pool).await;
    let notion = h.seed_mcp(h.seed.org_id, h.seed.user_id, "notion").await;
    let linear = h.seed_mcp(h.seed.org_id, h.seed.user_id, "linear").await;

    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let now = Utc::now();
    let base = ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: h.seed.agent_id,
        mcp_server: Some(notion),
        tool_name: "",
        started_at: now,
        is_error: false,
        error_message: None,
    };
    insert_tool_call(ToolCallSeed {
        tool_name: "pages.search",
        started_at: now - chrono::Duration::seconds(3),
        ..base
    })
    .await;
    insert_tool_call(ToolCallSeed {
        tool_name: "issues.search",
        mcp_server: Some(linear),
        started_at: now - chrono::Duration::seconds(2),
        ..base
    })
    .await;
    insert_tool_call(ToolCallSeed {
        tool_name: "projects.create",
        mcp_server: Some(linear),
        started_at: now - chrono::Duration::seconds(1),
        is_error: true,
        error_message: Some("403 forbidden"),
        ..base
    })
    .await;

    let uri = format!("/api/agents/{}/tool-calls", h.seed.agent_id.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);
    // Descending by started_at — newest first.
    assert_eq!(items[0]["tool_name"], "projects.create");
    assert_eq!(items[1]["tool_name"], "issues.search");
    assert_eq!(items[2]["tool_name"], "pages.search");

    // Per-row server projection lets the FE chip render without a second fetch.
    assert_eq!(items[0]["mcp_server_catalog_id"].as_str(), Some("linear"));
    assert_eq!(items[1]["mcp_server_catalog_id"].as_str(), Some("linear"));
    assert_eq!(items[2]["mcp_server_catalog_id"].as_str(), Some("notion"));
    assert_eq!(
        items[0]["mcp_server_id"].as_str(),
        Some(linear.as_uuid().to_string().as_str())
    );
    assert_eq!(
        items[2]["mcp_server_id"].as_str(),
        Some(notion.as_uuid().to_string().as_str())
    );

    // error_message follows the migration-27 CHECK: set only on errors.
    assert_eq!(items[0]["error_message"].as_str(), Some("403 forbidden"));
    assert_eq!(items[1]["error_message"], serde_json::Value::Null);
    assert_eq!(items[2]["error_message"], serde_json::Value::Null);

    assert_eq!(body["next_cursor"], serde_json::Value::Null);
}

#[sqlx::test]
async fn excludes_non_mcp_tool_calls(pool: PgPool) {
    // System tools (send_message, search_agents, …) record audit rows with
    // a null mcp_server_id. The per-agent "Recent activity" panel only
    // surfaces MCP traffic, so the endpoint must filter those rows out at
    // the SQL level — otherwise the cursor cap shrinks unpredictably and
    // mixed payloads leak through.
    let h = Harness::new(pool).await;
    let notion = h.seed_mcp(h.seed.org_id, h.seed.user_id, "notion").await;

    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let now = Utc::now();
    let base = ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: h.seed.agent_id,
        mcp_server: None,
        tool_name: "",
        started_at: now,
        is_error: false,
        error_message: None,
    };
    insert_tool_call(ToolCallSeed {
        tool_name: "send_message",
        started_at: now - chrono::Duration::seconds(3),
        ..base
    })
    .await;
    insert_tool_call(ToolCallSeed {
        tool_name: "search_agents",
        started_at: now - chrono::Duration::seconds(2),
        ..base
    })
    .await;
    insert_tool_call(ToolCallSeed {
        tool_name: "pages.search",
        mcp_server: Some(notion),
        started_at: now - chrono::Duration::seconds(1),
        ..base
    })
    .await;

    let uri = format!("/api/agents/{}/tool-calls", h.seed.agent_id.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["tool_name"], "pages.search");
    assert_eq!(items[0]["mcp_server_catalog_id"].as_str(), Some("notion"));
}

#[sqlx::test]
async fn excludes_calls_from_other_agents(pool: PgPool) {
    // Two agents in the same org: only the queried agent's rows come back,
    // even though both write to the same MCP server.
    let h = Harness::new(pool).await;
    let server = h.seed_mcp(h.seed.org_id, h.seed.user_id, "shared").await;

    let other_agent = h
        .agents
        .create(patom::agents::NewAgent {
            org_id: h.seed.org_id,
            name: patom::agents::AgentName::try_from("Bob").expect("name"),
            system_prompt: patom::agents::AgentSystemPrompt::try_from("you are bob")
                .expect("prompt"),
            description: patom::agents::AgentDescription::try_from("a helper").expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom::agents::AllowedMcpTools::default(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("create other agent")
        .id;

    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let now = Utc::now();
    insert_tool_call(ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: h.seed.agent_id,
        mcp_server: Some(server),
        tool_name: "mine",
        started_at: now,
        is_error: false,
        error_message: None,
    })
    .await;
    insert_tool_call(ToolCallSeed {
        pool: &h.state.pool,
        org: h.seed.org_id,
        session,
        request,
        agent: other_agent,
        mcp_server: Some(server),
        tool_name: "theirs",
        started_at: now - chrono::Duration::seconds(1),
        is_error: false,
        error_message: None,
    })
    .await;

    let uri = format!("/api/agents/{}/tool-calls", h.seed.agent_id.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["tool_name"], "mine");
}

#[sqlx::test]
async fn cursor_pagination_walks_backward_in_time(pool: PgPool) {
    let h = Harness::new(pool).await;
    let server = h.seed_mcp(h.seed.org_id, h.seed.user_id, "paged").await;
    let session = human_to_agent_session(
        h.state.sessions.as_ref(),
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let request = seed_prompt_request(&h.state.pool, session, h.seed.agent_id, h.seed.org_id).await;

    let base = Utc::now();
    for i in 0..5_i64 {
        let name = format!("tool_{i}");
        insert_tool_call(ToolCallSeed {
            pool: &h.state.pool,
            org: h.seed.org_id,
            session,
            request,
            agent: h.seed.agent_id,
            mcp_server: Some(server),
            tool_name: &name,
            started_at: base - chrono::Duration::seconds(i),
            is_error: false,
            error_message: None,
        })
        .await;
    }

    let uri = format!(
        "/api/agents/{}/tool-calls?limit=2",
        h.seed.agent_id.as_uuid()
    );
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["tool_name"], "tool_0");
    assert_eq!(items[1]["tool_name"], "tool_1");
    let cursor = body["next_cursor"]
        .as_str()
        .expect("cursor mid-pagination")
        .to_owned();

    let uri = format!(
        "/api/agents/{}/tool-calls?limit=2&before={}",
        h.seed.agent_id.as_uuid(),
        cursor,
    );
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["tool_name"], "tool_2");
    assert_eq!(items[1]["tool_name"], "tool_3");
    assert!(body["next_cursor"].is_string());

    let cursor = body["next_cursor"].as_str().expect("cursor").to_owned();
    let uri = format!(
        "/api/agents/{}/tool-calls?limit=2&before={}",
        h.seed.agent_id.as_uuid(),
        cursor,
    );
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let items = body["items"].as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["tool_name"], "tool_4");
    assert_eq!(body["next_cursor"], serde_json::Value::Null);
}

#[sqlx::test]
async fn cross_org_agent_returns_404(pool: PgPool) {
    let h = Harness::new(pool).await;
    // Seed an agent under a *different* org. The primary principal must
    // not see existence — same shape as `read_agent` cross-org access.
    let foreign = seed_principal(&h.state.pool, &h.state.jwt).await;
    let foreign_agent = h
        .agents
        .create(patom::agents::NewAgent {
            org_id: foreign.org_id,
            name: patom::agents::AgentName::try_from("Eve").expect("name"),
            system_prompt: patom::agents::AgentSystemPrompt::try_from("you are eve")
                .expect("prompt"),
            description: patom::agents::AgentDescription::try_from("eavesdrop").expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom::agents::AllowedMcpTools::default(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("create foreign agent")
        .id;

    let uri = format!("/api/agents/{}/tool-calls", foreign_agent.as_uuid());
    let (status, _) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn unauthenticated_request_returns_401(pool: PgPool) {
    let h = Harness::new(pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents/00000000-0000-0000-0000-000000000000/tool-calls")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
    let _ = h.mcp_store;
}
