//! End-to-end probe for the onboarding-complete signal on `PATCH /me/org`.
//!
//! Covers:
//!   1. A freshly seeded org reads as `onboarded: false` (the column was
//!      added in migration 57 and `seed_principal` inserts no value, so
//!      it stays NULL).
//!   2. `PATCH /me/org { onboarded: true }` (owner/admin) flips the
//!      column to NOT NULL and the next read returns `onboarded: true`.
//!   3. A second `PATCH /me/org { onboarded: true }` is idempotent —
//!      never un-marks, preserves the original timestamp (COALESCE
//!      contract in `update_org`).
//!   4. A member is forbidden from `PATCH /me/org` regardless of body
//!      shape (role gate enforced via `require_admin`).
//!   5. `PATCH /me/org` accepts `name` and `onboarded` in the same body
//!      — proves the wizard's "rename then later mark onboarded" flow
//!      works through one shared handler.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{JwtSigner, OrgId, Role, UserId};
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};

const ORG_PATH: &str = "/api/me/org";

struct Harness {
    state: AppState,
    owner: SeededPrincipal,
}

impl Harness {
    async fn new(pool: &PgPool) -> Self {
        let clock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();
        let leases: SharedLeaseManager = queue_impl;

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let sessions: SharedSessionStore =
            Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
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
            leases,
            responses,
            sessions,
            agents,
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
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

    async fn get_org(&self, principal: &SeededPrincipal) -> (axum::http::StatusCode, Value) {
        let res = router(self.state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(ORG_PATH)
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

    async fn patch_org(
        &self,
        principal: &SeededPrincipal,
        body: Value,
    ) -> (axum::http::StatusCode, Value) {
        let res = router(self.state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(ORG_PATH)
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
    let cookie_value = jwt.mint(user_id, Some(org)).expect("mint test jwt");
    SeededPrincipal {
        user_id,
        org_id: org,
        cookie_value,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn me_returns_onboarded_false_for_freshly_seeded_org(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let (status, body) = h.get_org(&h.owner).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        body.get("onboarded"),
        Some(&Value::Bool(false)),
        "freshly seeded org should not be onboarded (onboarded_at IS NULL); body = {body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn patch_org_marks_onboarded(pool: PgPool) {
    let h = Harness::new(&pool).await;

    // Confirm the precondition: still NULL.
    let (_, before) = h.get_org(&h.owner).await;
    assert_eq!(before.get("onboarded"), Some(&Value::Bool(false)));

    // Mark.
    let (status, after_patch) = h.patch_org(&h.owner, json!({ "onboarded": true })).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {after_patch}");
    assert_eq!(after_patch.get("onboarded"), Some(&Value::Bool(true)));

    // GET should agree.
    let (_, refetched) = h.get_org(&h.owner).await;
    assert_eq!(refetched.get("onboarded"), Some(&Value::Bool(true)));
}

#[sqlx::test(migrations = "./migrations")]
async fn patch_org_onboarded_is_idempotent_never_unmarks(pool: PgPool) {
    let h = Harness::new(&pool).await;

    // First mark stamps onboarded_at.
    let (_, _) = h.patch_org(&h.owner, json!({ "onboarded": true })).await;
    let first_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT onboarded_at FROM organizations WHERE id = $1",
        )
        .bind(h.owner.org_id)
        .fetch_one(&pool)
        .await
        .expect("read onboarded_at")
        .expect("first mark must have stamped a timestamp");

    // Second mark with `onboarded: true` must NOT change the stamp
    // (COALESCE preserves the original).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (_, _) = h.patch_org(&h.owner, json!({ "onboarded": true })).await;
    let second_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT onboarded_at FROM organizations WHERE id = $1",
        )
        .bind(h.owner.org_id)
        .fetch_one(&pool)
        .await
        .expect("read onboarded_at")
        .expect("second mark preserves the stamp");
    assert_eq!(
        first_at, second_at,
        "second mark must not overwrite the original onboarded_at"
    );

    // A patch with `onboarded: false` is a no-op on this column — the
    // PATCH path never un-marks.
    let (_, _) = h.patch_org(&h.owner, json!({ "onboarded": false })).await;
    let third_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT onboarded_at FROM organizations WHERE id = $1",
        )
        .bind(h.owner.org_id)
        .fetch_one(&pool)
        .await
        .expect("read onboarded_at")
        .expect("onboarded: false must not clear the stamp");
    assert_eq!(third_at, first_at, "onboarded: false must not un-mark");
}

#[sqlx::test(migrations = "./migrations")]
async fn patch_org_onboarded_requires_owner_or_admin(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let member = seed_member(&pool, h.owner.org_id, h.jwt()).await;

    let (status, body) = h.patch_org(&member, json!({ "onboarded": true })).await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "member must not be able to mark the org onboarded; body = {body}"
    );

    // And the column must still be NULL.
    let still_null: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT onboarded_at FROM organizations WHERE id = $1",
        )
        .bind(h.owner.org_id)
        .fetch_one(&pool)
        .await
        .expect("read onboarded_at");
    assert!(still_null.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn patch_org_accepts_name_and_onboarded_together(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let (status, body) = h
        .patch_org(&h.owner, json!({ "name": "Atlas Labs", "onboarded": true }))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    assert_eq!(body.get("name"), Some(&Value::String("Atlas Labs".into())));
    assert_eq!(body.get("onboarded"), Some(&Value::Bool(true)));

    // Sanity: also fully readable via GET.
    let (_, refetched) = h.get_org(&h.owner).await;
    assert_eq!(
        refetched.get("name"),
        Some(&Value::String("Atlas Labs".into()))
    );
    assert_eq!(refetched.get("onboarded"), Some(&Value::Bool(true)));
    // Owner role still surfaces correctly.
    assert_eq!(
        refetched.get("role").and_then(Value::as_str),
        Some(Role::Owner.as_str())
    );
}
