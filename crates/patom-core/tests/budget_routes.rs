//! End-to-end probe for the org spend-budget admin API (`/api/me/org/budget`).
//!
//! Covers the contracts in issue #92:
//!   1. GET returns the configured cap + warn threshold + current-period usage,
//!      and reads as unlimited (`null` cap) when no config row exists.
//!   2. PUT (owner/admin) sets and clears the cap + warn threshold.
//!   3. A member is forbidden from PUT (role gate) but may GET.
//!   4. Invalid values (`warn_threshold_bps` out of range, non-positive cap)
//!      are rejected at the boundary with 400.
//!   5. RLS isolates tenants: one principal's read/write never sees or touches
//!      another org's budget.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{JwtSigner, OrgId, UserId};
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::{seed_period_usage, set_budget};

const BUDGET_PATH: &str = "/api/me/org/budget";

struct BudgetHarness {
    state: AppState,
    /// A fresh owner principal in its own org.
    owner: SeededPrincipal,
}

impl BudgetHarness {
    async fn new(pool: &PgPool) -> Self {
        let clock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (_refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

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
        let owner = seed_principal(pool, &jwt).await;

        let state = AppState {
            queue,
            responses,
            agents,
            colleagues: Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                clock.clone(),
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
            index_html: std::sync::Arc::from(""),
            slack: None,
            assets: None,
            orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: Arc::new(patom::orgs::LogMailer),
            entitlements: Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self { state, owner }
    }

    fn jwt(&self) -> &JwtSigner {
        &self.state.jwt
    }

    /// GET the budget view as `principal`; returns (status, body json).
    async fn get_budget(&self, principal: &SeededPrincipal) -> (axum::http::StatusCode, Value) {
        let res = router(self.state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(BUDGET_PATH)
                    .header("cookie", principal.cookie_header())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("collect");
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// PUT a budget config as `principal`; returns (status, body json).
    async fn put_budget(
        &self,
        principal: &SeededPrincipal,
        body: Value,
    ) -> (axum::http::StatusCode, Value) {
        let res = router(self.state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(BUDGET_PATH)
                    .header("cookie", principal.cookie_header())
                    .header("X-CSRF-Token", principal.csrf_header())
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("collect");
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }
}

/// Insert a second user into `org` with `member` role and mint their cookie.
async fn seed_member(pool: &PgPool, org: OrgId, jwt: &JwtSigner) -> SeededPrincipal {
    let user_id = UserId::new();
    let email = format!(
        "member-{}@example.test",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(user_id)
        .bind(&email)
        .bind("Member User")
        .bind(now)
        .execute(pool)
        .await
        .expect("seed member user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(org)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed org member");
    let cookie_value = jwt.mint(user_id, Some(org)).expect("mint member jwt");
    SeededPrincipal {
        user_id,
        org_id: org,
        cookie_value,
    }
}

#[sqlx::test]
async fn get_budget_unlimited_when_no_config(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;
    let (status, body) = h.get_budget(&h.owner).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["monthly_cap_micro_usd"], Value::Null);
    assert_eq!(body["remaining_micro_usd"], Value::Null);
    assert_eq!(body["used_micro_usd"], json!(0));
    // No config row → the default warn threshold (80%).
    assert_eq!(body["warn_threshold_bps"], json!(8000));
}

#[sqlx::test]
async fn get_budget_returns_cap_and_current_usage(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;
    set_budget(&pool, h.owner.org_id, Some(5_000_000), 7000).await;
    seed_period_usage(&pool, h.owner.org_id, 1_000_000).await;

    let (status, body) = h.get_budget(&h.owner).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["monthly_cap_micro_usd"], json!(5_000_000));
    assert_eq!(body["warn_threshold_bps"], json!(7000));
    assert_eq!(body["used_micro_usd"], json!(1_000_000));
    // remaining = cap - used = 4_000_000.
    assert_eq!(body["remaining_micro_usd"], json!(4_000_000));
}

#[sqlx::test]
async fn put_budget_sets_and_clears_cap(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;

    // Set a cap + threshold.
    let (status, body) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": 5_000_000, "warn_threshold_bps": 9000 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["monthly_cap_micro_usd"], json!(5_000_000));
    assert_eq!(body["warn_threshold_bps"], json!(9000));

    // GET reflects it.
    let (_, after_set) = h.get_budget(&h.owner).await;
    assert_eq!(after_set["monthly_cap_micro_usd"], json!(5_000_000));

    // Clear the cap → unlimited.
    let (status, body) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": Value::Null, "warn_threshold_bps": 9000 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["monthly_cap_micro_usd"], Value::Null);
    assert_eq!(body["remaining_micro_usd"], Value::Null);

    let (_, after_clear) = h.get_budget(&h.owner).await;
    assert_eq!(after_clear["monthly_cap_micro_usd"], Value::Null);
}

#[sqlx::test]
async fn put_budget_member_is_forbidden_403(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;
    let member = seed_member(&pool, h.owner.org_id, h.jwt()).await;

    // A member may read.
    let (get_status, _) = h.get_budget(&member).await;
    assert_eq!(get_status, axum::http::StatusCode::OK);

    // But not write — the owner/admin role gate rejects it.
    let (status, _) = h
        .put_budget(
            &member,
            json!({ "monthly_cap_micro_usd": 5_000_000, "warn_threshold_bps": 8000 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn put_budget_invalid_value_400(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;

    // warn_threshold_bps out of range (0).
    let (status, _) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": 5_000_000, "warn_threshold_bps": 0 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    // warn_threshold_bps above 10000.
    let (status, _) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": 5_000_000, "warn_threshold_bps": 10_001 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    // Non-positive cap (0) — unlimited is `null`, never a stored zero.
    let (status, _) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": 0, "warn_threshold_bps": 8000 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn budget_is_tenant_isolated(pool: PgPool) {
    let h = BudgetHarness::new(&pool).await;
    // Second principal in a different org.
    let other = seed_principal(&pool, h.jwt()).await;

    // Owner sets a cap on org A.
    let (status, _) = h
        .put_budget(
            &h.owner,
            json!({ "monthly_cap_micro_usd": 5_000_000, "warn_threshold_bps": 8000 }),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // Org B's principal sees its own (unlimited) budget, never org A's cap.
    let (status, other_body) = h.get_budget(&other).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(other_body["monthly_cap_micro_usd"], Value::Null);

    // Org A still reads its own cap.
    let (_, owner_body) = h.get_budget(&h.owner).await;
    assert_eq!(owner_body["monthly_cap_micro_usd"], json!(5_000_000));

    // Org A's principal cannot write org B's row: org_budgets for B stays empty.
    let (b_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM org_budgets WHERE org_id = $1")
        .bind(other.org_id)
        .fetch_one(&pool)
        .await
        .expect("count org B budgets");
    assert_eq!(b_count, 0);
}
