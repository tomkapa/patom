//! End-to-end coverage for self-service workspace create + delete and the
//! org-less onboarding session (cloud build).
//!
//! Covers:
//!   1. `POST /me/orgs` (cloud) creates the org, makes the caller Owner,
//!      seeds a default agent, switches the session in, and `/me` then
//!      lists the new org as active + `onboarded: false`.
//!   2. The per-user owned-workspace cap (`MAX_ORGS_PER_USER`) returns 409.
//!   3. `POST /me/orgs` is 403 on a non-cloud (self-host) build.
//!   4. A malformed name is rejected 400 before any write.
//!   5. `DELETE /me/org` (owner) deletes the org + cascades and re-mints
//!      the session; a non-owner is 403.
//!   6. An org-less session: `/me` answers with `active_org_id: null`,
//!      and an org-scoped route (`GET /me/org`) rejects it 401.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{JwtSigner, UserId, limits::MAX_ORGS_PER_USER};
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
use common::auth::{SeededPrincipal, TEST_CSRF_TOKEN, seed_principal};

const ORGS_PATH: &str = "/api/me/orgs";
const ORG_PATH: &str = "/api/me/org";
const ME_PATH: &str = "/api/me";

struct Harness {
    state: AppState,
    owner: SeededPrincipal,
}

impl Harness {
    /// `cloud` toggles the build-mode flag the create endpoint + org-less
    /// callback branch on. Uses the permissive `UnlimitedEntitlements` (no
    /// signup grant).
    async fn new(pool: &PgPool, cloud: bool) -> Self {
        Self::with_entitlements(
            pool,
            cloud,
            Arc::new(patom::entitlements::UnlimitedEntitlements),
        )
        .await
    }

    async fn with_entitlements(
        pool: &PgPool,
        cloud: bool,
        entitlements: patom::entitlements::SharedEntitlements,
    ) -> Self {
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
            Arc::new(patom::mcp::PgMcpCatalogStore::new(
                pool.clone(),
                ::std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
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
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            billing: Arc::new(patom::billing::PgBillingService::with_entitlements(
                pool.clone(),
                clock.clone(),
                entitlements.clone(),
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
            cloud,
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
            lark: None,
            discord: None,
            assets: None,
            orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: Arc::new(patom::orgs::LogMailer),
            entitlements,
        };

        Self { state, owner }
    }

    async fn create_org(
        &self,
        cookie_header: &str,
        body: Value,
    ) -> (axum::http::StatusCode, Value) {
        self.send("POST", ORGS_PATH, cookie_header, Some(body))
            .await
    }

    async fn delete_org(&self, cookie_header: &str) -> (axum::http::StatusCode, Value) {
        self.send("DELETE", ORG_PATH, cookie_header, None).await
    }

    async fn get(&self, path: &str, cookie_header: &str) -> (axum::http::StatusCode, Value) {
        self.send("GET", path, cookie_header, None).await
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        cookie_header: &str,
        body: Option<Value>,
    ) -> (axum::http::StatusCode, Value) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", cookie_header);
        // State-changing requests carry the double-submit CSRF header.
        if method != "GET" {
            builder = builder
                .header("X-CSRF-Token", TEST_CSRF_TOKEN)
                .header("content-type", "application/json");
        }
        let req = builder
            .body(body.map_or_else(axum::body::Body::empty, |b| {
                axum::body::Body::from(b.to_string())
            }))
            .expect("request");
        let res = router(self.state.clone())
            .oneshot(req)
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

/// Cookie header carrying both the session JWT and the CSRF cookie for an
/// arbitrary token value (so we can build one for an org-less session,
/// which has no `SeededPrincipal`).
fn cookie_header(session_jwt: &str) -> String {
    format!(
        "{}={}; {}={}",
        patom::auth::limits::COOKIE_NAME,
        session_jwt,
        patom::auth::limits::CSRF_COOKIE_NAME,
        TEST_CSRF_TOKEN,
    )
}

/// Seed a bare `users` row (no org) and mint an **org-less** session
/// cookie for them — the state a brand-new cloud user is in before
/// onboarding.
async fn seed_org_less(pool: &PgPool, jwt: &JwtSigner) -> (UserId, String) {
    let user_id = UserId::new();
    let email = format!(
        "orgless-{}@example.test",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(user_id)
        .bind(&email)
        .bind("Org Less")
        .bind(now)
        .execute(pool)
        .await
        .expect("seed org-less user");
    let token = jwt.mint(user_id, None).expect("mint org-less jwt");
    (user_id, cookie_header(&token))
}

#[sqlx::test(migrations = "./migrations")]
async fn create_org_cloud_makes_owner_with_default_agent(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    // One org per account (#121): the caller must own no workspace yet, so
    // start from a fresh org-less session rather than the pre-seeded owner.
    let (user_id, cookie) = seed_org_less(&pool, &h.state.jwt).await;
    let (status, body) = h.create_org(&cookie, json!({ "name": "Atlas Labs" })).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body = {body}");
    assert_eq!(body.get("role").and_then(Value::as_str), Some("owner"));
    let new_org = body
        .get("active_org_id")
        .and_then(Value::as_str)
        .expect("active_org_id present");
    let new_org_uuid = uuid::Uuid::parse_str(new_org).expect("uuid");

    // Owner membership row exists for the caller in the new org.
    let owner_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = $2 AND role = 'owner'",
    )
    .bind(new_org_uuid)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner");
    assert_eq!(owner_count, 1, "caller must own the new org");

    // A default agent was seeded so the workspace is usable.
    let agent_count: i64 = sqlx::query_scalar("SELECT count(*) FROM agents WHERE org_id = $1")
        .bind(new_org_uuid)
        .fetch_one(&pool)
        .await
        .expect("count agents");
    assert!(agent_count >= 1, "new org must have a seeded default agent");
}

/// One org per account (#121): an identity may own exactly one self-service
/// workspace. The seeded owner already owns their single org, so a second
/// `POST /me/orgs` is the (cap+1)-th create and must be rejected 409.
#[sqlx::test(migrations = "./migrations")]
async fn second_owned_org_per_account_returns_409(pool: PgPool) {
    // Guard the policy constant itself so this test can't silently pass for
    // the wrong reason if the cap is later raised.
    assert_eq!(MAX_ORGS_PER_USER, 1, "launch policy is one org per account");

    let h = Harness::new(&pool, true).await;
    // The seeded owner already owns exactly one org — the cap.
    let (status, body) = h
        .create_org(&h.owner.cookie_header(), json!({ "name": "One Too Many" }))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "a second owned workspace must be rejected (one org per account); body = {body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn create_org_is_forbidden_on_self_host_build(pool: PgPool) {
    let h = Harness::new(&pool, false).await;
    let (status, body) = h
        .create_org(&h.owner.cookie_header(), json!({ "name": "Nope Inc" }))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "create must be 403 on a non-cloud build; body = {body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn create_org_rejects_blank_name(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    let (status, _) = h
        .create_org(&h.owner.cookie_header(), json!({ "name": "   " }))
        .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_org_owner_removes_and_cascades(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    let org_id = h.owner.org_id;

    let (status, body) = h.delete_org(&h.owner.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    // The seeded owner had exactly one org → org-less afterward.
    assert_eq!(
        body.get("active_org_id"),
        Some(&Value::Null),
        "deleting the only org lands the user org-less"
    );

    let org_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .expect("count org");
    assert_eq!(org_rows, 0, "org row must be gone");

    let member_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM org_members WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .expect("count members");
    assert_eq!(member_rows, 0, "membership rows must cascade-delete");
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_org_non_owner_is_forbidden(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    // Seed a plain member of the owner's org and mint their cookie.
    let member_id = UserId::new();
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(member_id)
        .bind(format!("m-{}@example.test", &uuid::Uuid::new_v4().simple().to_string()[..6]))
        .bind("Member")
        .bind(now)
        .execute(&pool)
        .await
        .expect("seed member user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(h.owner.org_id)
    .bind(member_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("seed membership");
    let member_cookie = cookie_header(
        &h.state
            .jwt
            .mint(member_id, Some(h.owner.org_id))
            .expect("mint member jwt"),
    );

    let (status, _) = h.delete_org(&member_cookie).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    // The org must still exist.
    let org_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations WHERE id = $1")
        .bind(h.owner.org_id)
        .fetch_one(&pool)
        .await
        .expect("count org");
    assert_eq!(org_rows, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn org_less_session_me_is_orgless_but_org_scoped_route_401s(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    let (_user, cookie) = seed_org_less(&pool, &h.state.jwt).await;

    // /me answers for an org-less user with null active org + empty orgs.
    let (status, body) = h.get(ME_PATH, &cookie).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    assert_eq!(body.get("active_org_id"), Some(&Value::Null));
    assert_eq!(body.get("role"), Some(&Value::Null));
    assert_eq!(
        body.get("orgs").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );

    // An org-scoped route rejects the org-less token before the handler.
    let (status, _) = h.get(ORG_PATH, &cookie).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}

const ORG_RULE_PATH: &str = "/api/me/org/rule";

/// `GET /me/org` surfaces the org's `default_rule` so the settings editor
/// can seed itself from the same payload it already reads (parity with
/// `default_language`). The round-trip also covers the `set_org_rule`
/// write route, which had no integration coverage before.
#[sqlx::test(migrations = "./migrations")]
async fn org_details_surface_default_rule_round_trip(pool: PgPool) {
    let h = Harness::new(&pool, true).await;
    let cookie = h.owner.cookie_header();

    // No rule configured yet → the field is present and null.
    let (status, body) = h.get(ORG_PATH, &cookie).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    assert_eq!(body.get("default_rule"), Some(&Value::Null));

    // Owner sets a rule.
    let (status, _) = h
        .send(
            "PATCH",
            ORG_RULE_PATH,
            &cookie,
            Some(json!({ "rule": "Be concise." })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // GET /me/org now echoes it back, ready to seed the textarea.
    let (status, body) = h.get(ORG_PATH, &cookie).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    assert_eq!(
        body.get("default_rule").and_then(Value::as_str),
        Some("Be concise.")
    );

    // A whitespace-only body clears the rule back to null.
    let (status, _) = h
        .send(
            "PATCH",
            ORG_RULE_PATH,
            &cookie,
            Some(json!({ "rule": "   " })),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let (status, body) = h.get(ORG_PATH, &cookie).await;
    assert_eq!(status, axum::http::StatusCode::OK, "body = {body}");
    assert_eq!(body.get("default_rule"), Some(&Value::Null));
}

// ─────────────────────────────────────────────────────────────────────
// #154 S8 — signup credit grant on org creation.
// ─────────────────────────────────────────────────────────────────────

/// Read the materialized credit balance for an org (RLS-bypassed owner pool).
async fn org_credits(pool: &PgPool, org: uuid::Uuid) -> Option<(i64, i64, i64)> {
    common::billing::read_org_credits(pool, patom::auth::OrgId::from(org)).await
}

#[sqlx::test(migrations = "./migrations")]
async fn create_org_fires_signup_grant_under_cloud_policy(pool: PgPool) {
    let h =
        Harness::with_entitlements(&pool, true, common::billing::signup_grant_policy(2_000_000))
            .await;
    // One org per account (#121): create from a fresh org-less session.
    let (_user_id, cookie) = seed_org_less(&pool, &h.state.jwt).await;
    let (status, body) = h
        .create_org(&cookie, json!({ "name": "Funded Labs" }))
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body = {body}");
    let new_org = uuid::Uuid::parse_str(
        body.get("active_org_id")
            .and_then(Value::as_str)
            .expect("active_org_id"),
    )
    .expect("uuid");

    // The org is seeded with the $2 signup grant.
    let (balance, granted, used) = org_credits(&pool, new_org).await.expect("credits row");
    assert_eq!(balance, 2_000_000);
    assert_eq!(granted, 2_000_000);
    assert_eq!(used, 0);

    // Exactly one signup_bonus ledger entry, keyed deterministically.
    let (entries, key): (i64, String) = sqlx::query_as(
        "SELECT count(*)::bigint, max(idempotency_key) FROM org_credit_ledger \
         WHERE org_id = $1 AND reason = 'signup_bonus'",
    )
    .bind(new_org)
    .fetch_one(&pool)
    .await
    .expect("ledger");
    assert_eq!(entries, 1);
    assert_eq!(key, format!("signup:{new_org}"));
}

#[sqlx::test(migrations = "./migrations")]
async fn signup_grant_is_idempotent_on_replay(pool: PgPool) {
    // A retry of the grant (same deterministic key) must not double-credit.
    let h =
        Harness::with_entitlements(&pool, true, common::billing::signup_grant_policy(2_000_000))
            .await;
    // One org per account (#121): create from a fresh org-less session.
    let (_user_id, cookie) = seed_org_less(&pool, &h.state.jwt).await;
    let (_, body) = h.create_org(&cookie, json!({ "name": "Retry Labs" })).await;
    let new_org = uuid::Uuid::parse_str(
        body.get("active_org_id")
            .and_then(Value::as_str)
            .expect("active_org_id"),
    )
    .expect("uuid");

    // Re-fire the exact signup grant the handler issued.
    let key = patom::runtime::IdempotencyKey::try_from(format!("signup:{new_org}")).expect("key");
    h.state
        .billing
        .grant_credit(
            patom::auth::OrgId::from(new_org),
            patom::billing::GrantAmount::try_from(2_000_000).expect("amount"),
            patom::billing::LedgerReason::SignupBonus,
            &key,
            None,
        )
        .await
        .expect("replay grant");

    let (balance, granted, _used) = org_credits(&pool, new_org).await.expect("credits row");
    assert_eq!(balance, 2_000_000, "replay must not double-credit");
    assert_eq!(granted, 2_000_000);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_org_grants_nothing_under_oss_default(pool: PgPool) {
    // UnlimitedEntitlements → signup_grant is None → no org_credits row at all.
    let h = Harness::new(&pool, true).await;
    // One org per account (#121): create from a fresh org-less session.
    let (_user_id, cookie) = seed_org_less(&pool, &h.state.jwt).await;
    let (status, body) = h
        .create_org(&cookie, json!({ "name": "Self Host Inc" }))
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body = {body}");
    let new_org = uuid::Uuid::parse_str(
        body.get("active_org_id")
            .and_then(Value::as_str)
            .expect("active_org_id"),
    )
    .expect("uuid");
    assert!(
        org_credits(&pool, new_org).await.is_none(),
        "OSS default must not seed credits"
    );
}
