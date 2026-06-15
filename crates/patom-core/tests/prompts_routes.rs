//! Integration tests for `POST /prompts` in the Slack-parity model.
//!
//! Tags drive triggers: zero tags = a plain post (no trigger, `request_id`
//! null); each agent tag enqueues its own trigger + fresh DAG; human tags
//! render only. A DM's agent counterpart is the implicit receiver of an
//! untagged message; an explicit agent tag overrides it for that message
//! only. There is no default agent.

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
            Arc::new(patom::mcp::PgMcpCatalogStore::new(
                pool.clone(),
                ::std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            ));
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
        // Pin the principal to the same org as `seed.agent_id`. The
        // cross-org isolation path is exercised in `tests/auth_agents.rs`.
        let seeded = common::auth::principal_for_default_org(seed.user_id, seed.org_id, &jwt);
        let state = AppState {
            queue: queue.clone(),
            responses,
            agents: agents.clone(),
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            billing: std::sync::Arc::new(patom::billing::PgBillingService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            provider_credentials: common::pg::provider_credentials_store(pool.clone()),
            provider_refresh: patom::provider::ProviderRefreshTrigger::disconnected(),
            providers: std::sync::Arc::new(patom::provider::ProviderRegistry::builder().build()),
            provider_overlay: patom::provider::OrgProviderOverlay::empty(),
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
            index_html: std::sync::Arc::from(""),
            slack: None,
            lark: None,
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

/// A fresh DM root tagging its counterpart routes there; the untagged
/// continuation stays with the counterpart (implicit DM receiver), with no
/// default-agent fallback anywhere.
#[sqlx::test]
async fn dm_root_then_untagged_continuation_routes_to_counterpart(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let translator = h.create_agent("translator").await;

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "counterpart": {"kind": "agent", "id": translator.as_uuid()},
            "content": "hi translator",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let thread_id = body["thread_id"].as_str().expect("thread_id").to_owned();
    assert_eq!(
        body["triggered_agent_ids"].as_array().map(Vec::len),
        Some(1),
        "DM root wakes the counterpart"
    );
    assert_receiver_agent(
        &h.pool,
        body["request_id"].as_str().expect("rid"),
        translator,
    )
    .await;

    // Untagged continuation in the SAME thread → still the counterpart.
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

/// Tagging a third agent inside a DM routes that message to the tagged agent
/// ONLY; the next untagged message returns to the DM counterpart — never a
/// `last_agent` capture by the third agent (R5).
#[sqlx::test]
async fn dm_untagged_after_third_agent_tag_returns_to_counterpart(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let counterpart = h.create_agent("counterpart").await;
    let third = h.create_agent("third-wheel").await;

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "counterpart": {"kind": "agent", "id": counterpart.as_uuid()},
            "content": "hello",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    let thread_id = body["thread_id"].as_str().expect("thread_id").to_owned();

    // Explicit tag of the third agent → that message wakes ONLY the third.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "thread_id": thread_id,
            "tags": [{"kind": "agent", "id": third.as_uuid()}],
            "content": "@third-wheel take a look",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        body["triggered_agent_ids"].as_array().map(Vec::len),
        Some(1),
        "an explicit tag overrides the implicit counterpart"
    );
    assert_receiver_agent(&h.pool, body["request_id"].as_str().expect("rid"), third).await;

    // The next untagged message returns to the counterpart, not the third.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "thread_id": thread_id,
            "content": "and what do you think?",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert_receiver_agent(
        &h.pool,
        body["request_id"].as_str().expect("rid"),
        counterpart,
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

/// Seed a channel the harness user is a member of, bypassing the HTTP
/// channel routes (this file pins the prompts contract, not channel CRUD).
async fn seed_channel(h: &PromptsHarness) -> Uuid {
    let channel = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, 'war-room', $3, now())",
    )
    .bind(channel)
    .bind(h.seed.org_id)
    .bind(h.seed.user_id)
    .execute(&h.pool)
    .await
    .expect("seed channel");
    sqlx::query(
        "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
         VALUES ($1, $2, $3, now())",
    )
    .bind(channel)
    .bind(h.seed.user_id)
    .bind(h.seed.org_id)
    .execute(&h.pool)
    .await
    .expect("seed membership");
    channel
}

/// An untagged channel post is a plain message: 202, `request_id` null, the
/// posted row lands (carrying the client idempotency key), and NO trigger
/// row exists.
#[sqlx::test]
async fn untagged_channel_post_appends_without_trigger(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let channel = seed_channel(&h).await;
    let key = Uuid::new_v4().to_string();

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "content": "just thinking out loud",
            "idempotency_key": key,
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert!(body["request_id"].is_null(), "no trigger => no request id");
    assert!(body["status"].is_null());
    assert_eq!(
        body["triggered_agent_ids"].as_array().map(Vec::len),
        Some(0)
    );

    let (triggers,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM prompt_requests WHERE org_id = $1")
            .bind(h.seed.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("count");
    assert_eq!(triggers, 0, "an untagged post enqueues nothing");

    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE org_id = $1 AND idempotency_key = $2",
    )
    .bind(h.seed.org_id)
    .bind(&key)
    .fetch_one(&h.pool)
    .await
    .expect("count posted");
    assert_eq!(posted, 1, "the posted row landed with the client key");

    // A retry of the same key converges on the same row (200, same thread).
    let (status, retry) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "content": "just thinking out loud",
            "idempotency_key": key,
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "retry => 200");
    assert_eq!(retry["thread_id"], body["thread_id"]);
    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE org_id = $1 AND idempotency_key = $2",
    )
    .bind(h.seed.org_id)
    .bind(&key)
    .fetch_one(&h.pool)
    .await
    .expect("recount posted");
    assert_eq!(posted, 1, "retry must not double-post");
}

/// `@X @Y` in one message: two triggers, two fresh DAG budgets, one posted row.
#[sqlx::test]
async fn two_tags_one_message_mint_two_triggers_and_two_dags(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let channel = seed_channel(&h).await;
    let x = h.create_agent("agent-x").await;
    let y = h.create_agent("agent-y").await;

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "tags": [
                {"kind": "agent", "id": x.as_uuid()},
                {"kind": "agent", "id": y.as_uuid()},
            ],
            "content": "@agent-x @agent-y both of you",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        body["triggered_agent_ids"].as_array().map(Vec::len),
        Some(2)
    );

    let (triggers,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM prompt_requests WHERE org_id = $1")
            .bind(h.seed.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("count triggers");
    assert_eq!(triggers, 2, "one trigger per tagged agent");

    let (dags,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM prompt_request_dags WHERE org_id = $1")
            .bind(h.seed.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("count dags");
    assert_eq!(dags, 2, "each tag mints its own DAG budget");

    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE org_id = $1 AND kind = 'posted'",
    )
    .bind(h.seed.org_id)
    .fetch_one(&h.pool)
    .await
    .expect("count posted");
    assert_eq!(posted, 1, "one message, regardless of tag count");
}

/// Tagging a human posts + stamps the receiver but enqueues nothing.
#[sqlx::test]
async fn human_tag_posts_without_trigger(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let channel = seed_channel(&h).await;
    let key = Uuid::new_v4().to_string();

    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "tags": [{"kind": "human", "id": h.seed.user_id}],
            "content": "@Seeded Test User ping",
            "idempotency_key": key,
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert!(body["request_id"].is_null(), "human tags never trigger");

    let (receiver_user,): (Option<Uuid>,) = sqlx::query_as(
        "SELECT rc.user_id FROM thread_messages m \
         JOIN colleagues rc ON rc.id = m.receiver_colleague_id \
         WHERE m.org_id = $1 AND m.idempotency_key = $2",
    )
    .bind(h.seed.org_id)
    .bind(&key)
    .fetch_one(&h.pool)
    .await
    .expect("receiver row");
    assert_eq!(
        receiver_user,
        Some(h.seed.user_id.as_uuid()),
        "the posted row is addressed to the tagged human"
    );
}

/// Admission gate: an over-budget org gets a 429 on a trigger-bearing submit
/// with NOTHING persisted; an untagged post still passes (it costs nothing).
#[sqlx::test]
async fn over_budget_org_is_rejected_with_429(pool: PgPool) {
    let h = PromptsHarness::new(pool.clone()).await;
    let channel = seed_channel(&h).await;
    let agent = h.create_agent("worker-bee").await;

    // Cap the org at 1 micro-USD and record 1000 already spent this period.
    common::pg::set_billing(&h.pool, h.seed.org_id, Some(1), 8000).await;
    common::pg::seed_period_usage(&h.pool, h.seed.org_id, 1000).await;

    let (status, _body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "tags": [{"kind": "agent", "id": agent.as_uuid()}],
            "content": "should be rejected",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);

    // Nothing was persisted — the gate ran before the thread create/append.
    let (requests,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM prompt_requests WHERE org_id = $1")
            .bind(h.seed.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("count requests");
    assert_eq!(requests, 0, "over-budget prompt must not enqueue work");
    let (messages,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM thread_messages WHERE org_id = $1")
            .bind(h.seed.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("count messages");
    assert_eq!(messages, 0, "over-budget prompt must not post");

    // An untagged post costs nothing and passes the gate.
    let (status, body) = post_json(
        h.state.clone(),
        "/api/prompts",
        serde_json::json!({
            "channel_id": channel,
            "content": "plain post while over budget",
            "idempotency_key": Uuid::new_v4().to_string(),
        }),
        &h.auth_cookie,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    assert!(body["request_id"].is_null());
}
