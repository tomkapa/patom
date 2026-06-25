//! Regression test for `GET /agents/{id}/turns`, driven through the live
//! axum router.
//!
//! `turn_metrics.kind` is the [`MetricKind`] superset
//! (`normal`/`reflection`/`resolution`/`compaction`): migration 84 widened the
//! column's CHECK so a #182 context-compaction fold can be metered as a
//! `kind='compaction'` row. The timeline read path must decode that column as
//! `MetricKind`, not the narrower dispatch enum `RequestKind` — otherwise a
//! single compaction row makes the whole timeline 500 with
//! `ColumnDecode { index: "kind", source: "invariant: unknown RequestKind
//! \"compaction\"" }`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use chrono::Utc;
use patom::agent_core::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, PgTurnMetricsStore, StopReasonLabel, TurnMetricsId,
    TurnMetricsRow, TurnMetricsStore,
};
use patom::agents::{PromptVersionId, SharedAgentStore};
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::provider::{Model, ProviderId};
use patom::runtime::{
    MetricKind, PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, principal_for_default_org};
use common::pg::{seed_agent_thread_state, seed_prompt_request, seed_tenant};

struct Harness {
    seed: common::pg::Seed,
    state: AppState,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    agents: SharedAgentStore,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl Harness {
    async fn new(pool: PgPool) -> Self {
        let seed = seed_tenant(&pool).await;
        let clock: SharedClock = SystemClock::shared();

        let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));

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
                Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
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
        let primary = principal_for_default_org(seed.user_id, seed.org_id, &jwt);
        let state = AppState {
            queue,
            responses,
            agents: agents.clone(),
            colleagues: Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            billing: Arc::new(patom::billing::PgBillingService::new(
                pool.clone(),
                SystemClock::shared(),
            )),
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog,
            mcp_refresh,
            provider_credentials: common::pg::provider_credentials_store(pool.clone()),
            provider_refresh: patom::provider::ProviderRefreshTrigger::disconnected(),
            providers: Arc::new(patom::provider::ProviderRegistry::builder().build()),
            provider_overlay: patom::provider::OrgProviderOverlay::empty(),
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
            index_html: Arc::from(""),
            slack: None,
            lark: None,
            discord: None,
            assets: None,
            orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: Arc::new(patom::orgs::LogMailer),
            entitlements: Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            seed,
            state,
            primary,
            agents,
            refresher,
        }
    }
}

async fn prompt_version_id(pool: &PgPool, agent_id: patom::agents::AgentId) -> PromptVersionId {
    sqlx::query_scalar(
        "SELECT id FROM agent_prompt_versions WHERE agent_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
    .expect("agent has a prompt version")
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

/// Record one `turn_metrics` row of the given `kind` for the seeded agent,
/// re-using the real recorder so column drift can't silently diverge from prod.
async fn record_turn(
    pool: &PgPool,
    h: &Harness,
    state_id: patom::threads::AgentThreadId,
    request: patom::runtime::PromptRequestId,
    pv: PromptVersionId,
    kind: MetricKind,
) {
    let store = PgTurnMetricsStore::new(pool.clone(), SystemClock::shared());
    store
        .record(TurnMetricsRow {
            id: TurnMetricsId::new(),
            request_id: request,
            org_id: h.seed.org_id,
            state_id,
            agent_id: h.seed.agent_id,
            prompt_version_id: pv,
            kind,
            model: Model::try_from("test-model").expect("catalog model"),
            provider: ProviderId::Anthropic,
            input_tokens: InputTokens::try_from(1_200_u32).expect("tokens"),
            output_tokens: OutputTokens::try_from(300_u32).expect("tokens"),
            cache_creation_tokens: None,
            cache_read_tokens: None,
            duration_ms: DurationMs::saturating_from_millis(42),
            stop_reason: StopReasonLabel::from_truncated("end_turn"),
            started_at: Utc::now(),
        })
        .await
        .expect("record turn metric");
}

/// A `kind='compaction'` turn_metrics row (migration 84) must not crash the
/// timeline — it is a real metered LLM call and belongs in the list. Before the
/// fix the read row decoded `kind` as `RequestKind` (no `Compaction` variant)
/// and the whole endpoint 500'd on the first compaction fold an agent did.
#[sqlx::test]
async fn compaction_turn_appears_in_timeline(pool: PgPool) {
    let h = Harness::new(pool.clone()).await;
    let state_id = seed_agent_thread_state(&pool, h.seed.org_id, h.seed.agent_id).await;
    let request = seed_prompt_request(
        &pool,
        state_id,
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let pv = prompt_version_id(&pool, h.seed.agent_id).await;
    record_turn(&pool, &h, state_id, request, pv, MetricKind::Compaction).await;

    let uri = format!("/api/agents/{}/turns", h.seed.agent_id.as_uuid());
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "compaction");
}

/// The token-spend chart aggregation must count `compaction` folds in its
/// per-bucket `by_kind` breakdown — otherwise metered compaction spend is
/// invisible in the chart even though the timeline now lists it.
#[sqlx::test]
async fn timeseries_by_kind_counts_compaction(pool: PgPool) {
    let h = Harness::new(pool.clone()).await;
    let state_id = seed_agent_thread_state(&pool, h.seed.org_id, h.seed.agent_id).await;
    let request = seed_prompt_request(
        &pool,
        state_id,
        h.seed.agent_id,
        h.seed.org_id,
        h.seed.user_id,
    )
    .await;
    let pv = prompt_version_id(&pool, h.seed.agent_id).await;
    record_turn(&pool, &h, state_id, request, pv, MetricKind::Normal).await;
    record_turn(&pool, &h, state_id, request, pv, MetricKind::Compaction).await;
    record_turn(&pool, &h, state_id, request, pv, MetricKind::Compaction).await;

    let uri = format!(
        "/api/agents/{}/metrics/timeseries",
        h.seed.agent_id.as_uuid()
    );
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    let buckets = body["buckets"].as_array().expect("buckets array");
    let normal: i64 = buckets
        .iter()
        .map(|b| b["by_kind"]["normal"].as_i64().unwrap_or(0))
        .sum();
    let compaction: i64 = buckets
        .iter()
        .map(|b| b["by_kind"]["compaction"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(normal, 1, "one normal turn");
    assert_eq!(compaction, 2, "two compaction folds counted in by_kind");
}
