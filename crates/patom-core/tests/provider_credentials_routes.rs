//! End-to-end probe for the BYO provider-credential admin API (#141):
//! `/api/me/org/provider-credentials`.
//!
//!   1. GET lists every provider, `not_set` until a key is stored, never any
//!      plaintext — only a masked suffix.
//!   2. PUT (owner/admin) stores a key (→ `active` + masked) and rotates it;
//!      `default_model` on the first key sets the per-org default.
//!   3. A member is forbidden from PUT/DELETE (role gate) but may GET.
//!   4. DELETE removes a key (→ `not_set`).
//!   5. RLS / per-org isolation: one principal never sees another org's keys.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{JwtSigner, OrgId, UserId};
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::provider::Model;
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

const BASE: &str = "/api/me/org/provider-credentials";

struct Harness {
    state: AppState,
    owner: SeededPrincipal,
}

impl Harness {
    async fn new(pool: &PgPool) -> Self {
        let clock = SystemClock::shared();
        let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;
        let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(
                pool.clone(),
                Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            ));
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
            billing: Arc::new(patom::billing::PgBillingService::new(
                pool.clone(),
                clock.clone(),
            )),
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            provider_credentials: common::pg::provider_credentials_store(pool.clone()),
            provider_refresh: patom::provider::ProviderRefreshTrigger::disconnected(),
            providers: std::sync::Arc::new(patom::provider::ProviderRegistry::builder().build()),
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
        Self { state, owner }
    }

    async fn get(&self, who: &SeededPrincipal) -> (axum::http::StatusCode, Value) {
        let res = router(self.state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(BASE)
                    .header("cookie", who.cookie_header())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        collect(res).await
    }

    async fn send(
        &self,
        method: &str,
        who: &SeededPrincipal,
        path: &str,
        body: Option<Value>,
    ) -> (axum::http::StatusCode, Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(format!("{BASE}{path}"))
            .header("cookie", who.cookie_header())
            .header("X-CSRF-Token", who.csrf_header());
        let req = if body.is_some() {
            req.header("content-type", "application/json")
        } else {
            req
        };
        let body = body.map_or_else(axum::body::Body::empty, |v| {
            axum::body::Body::from(v.to_string())
        });
        let res = router(self.state.clone())
            .oneshot(req.body(body).expect("request"))
            .await
            .expect("response");
        collect(res).await
    }
}

async fn collect(res: axum::response::Response) -> (axum::http::StatusCode, Value) {
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// A second user in `org` with `member` role + their cookie.
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
        .bind("Member")
        .bind(now)
        .execute(pool)
        .await
        .expect("insert member user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(org)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert membership");
    let cookie_value = jwt.mint(user_id, Some(org)).expect("mint member jwt");
    SeededPrincipal {
        user_id,
        org_id: org,
        cookie_value,
    }
}

fn provider_view<'a>(list: &'a Value, provider: &str) -> &'a Value {
    list.as_array()
        .expect("array")
        .iter()
        .find(|v| v["provider"] == provider)
        .expect("provider present")
}

#[sqlx::test]
async fn list_starts_all_not_set(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let (status, body) = h.get(&h.owner).await;
    assert_eq!(status, 200);
    for p in ["anthropic", "openai", "deepseek"] {
        assert_eq!(provider_view(&body, p)["status"], "not_set");
        assert_eq!(provider_view(&body, p)["masked_key"], Value::Null);
    }
}

#[sqlx::test]
async fn put_stores_key_masked_and_sets_first_key_default(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let (status, _) = h
        .send(
            "PUT",
            &h.owner,
            "/anthropic",
            Some(json!({ "api_key": "sk-ant-secret-9876", "default_model": "claude-sonnet-4-6" })),
        )
        .await;
    assert_eq!(status, 204);

    let (_, body) = h.get(&h.owner).await;
    let anthropic = provider_view(&body, "anthropic");
    assert_eq!(anthropic["status"], "active");
    let masked = anthropic["masked_key"].as_str().expect("masked");
    assert!(masked.ends_with("9876"), "keeps suffix: {masked}");
    assert!(!masked.contains("secret"), "hides body: {masked}");

    // First-key default model persisted — surfaced via the same read the
    // overlay refresher uses.
    let defaults = h
        .state
        .provider_credentials
        .list_default_models()
        .await
        .expect("list defaults");
    assert_eq!(
        defaults,
        vec![(
            h.owner.org_id,
            Model::try_from("claude-sonnet-4-6").unwrap()
        )]
    );
}

#[sqlx::test]
async fn rotate_replaces_key(pool: PgPool) {
    let h = Harness::new(&pool).await;
    h.send(
        "PUT",
        &h.owner,
        "/openai",
        Some(json!({ "api_key": "sk-old-1111" })),
    )
    .await;
    h.send(
        "PUT",
        &h.owner,
        "/openai",
        Some(json!({ "api_key": "sk-new-2222" })),
    )
    .await;
    let (_, body) = h.get(&h.owner).await;
    let masked = provider_view(&body, "openai")["masked_key"]
        .as_str()
        .expect("masked");
    assert!(masked.ends_with("2222"), "rotated: {masked}");
}

#[sqlx::test]
async fn delete_clears_key(pool: PgPool) {
    let h = Harness::new(&pool).await;
    h.send(
        "PUT",
        &h.owner,
        "/deepseek",
        Some(json!({ "api_key": "sk-ds-1234" })),
    )
    .await;
    let (status, _) = h.send("DELETE", &h.owner, "/deepseek", None).await;
    assert_eq!(status, 204);
    let (_, body) = h.get(&h.owner).await;
    assert_eq!(provider_view(&body, "deepseek")["status"], "not_set");
}

#[sqlx::test]
async fn member_is_forbidden_from_put_and_delete(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let member = seed_member(&pool, h.owner.org_id, &h.state.jwt).await;

    let (put_status, _) = h
        .send(
            "PUT",
            &member,
            "/anthropic",
            Some(json!({ "api_key": "sk-x-0000" })),
        )
        .await;
    assert_eq!(put_status, 403, "member cannot store keys");

    let (del_status, _) = h.send("DELETE", &member, "/anthropic", None).await;
    assert_eq!(del_status, 403, "member cannot delete keys");

    // But a member may read the masked status.
    let (get_status, _) = h.get(&member).await;
    assert_eq!(get_status, 200);
}

#[sqlx::test]
async fn unknown_provider_is_rejected(pool: PgPool) {
    let h = Harness::new(&pool).await;
    let (status, _) = h
        .send(
            "PUT",
            &h.owner,
            "/made-up",
            Some(json!({ "api_key": "sk-x-0000" })),
        )
        .await;
    assert_eq!(status, 400, "unknown provider id rejected at the boundary");
}

#[sqlx::test]
async fn orgs_are_isolated(pool: PgPool) {
    let h = Harness::new(&pool).await;
    h.send(
        "PUT",
        &h.owner,
        "/anthropic",
        Some(json!({ "api_key": "sk-owner-a" })),
    )
    .await;

    // A second principal in a different org sees no keys.
    let other = seed_principal(&pool, &h.state.jwt).await;
    let (_, body) = h.get(&other).await;
    assert_eq!(provider_view(&body, "anthropic")["status"], "not_set");
}
