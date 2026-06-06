//! HTTP integration test for `POST /api/me/invites/accept` — the
//! token-redeem endpoint behind the `/i/{slug}/{token}` invite link.
//!
//! Boots an `AppState` against a fresh schema-per-test pool, builds the
//! production `router`, and pokes it via `tower::ServiceExt::oneshot`.
//! Asserts that an authenticated caller redeeming a pending invite token
//! lands in the inviting org (the response switches their active org and
//! re-mints the session cookie).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::auth::{
    Email, IssuerUrl, Language, OidcProfile, OidcSubject, PgUserStore, Role, UserStore,
};
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::orgs::{OrgStore, PgOrgStore};
use sqlx::PgPool;
use tower::ServiceExt;

mod common;

/// Build the production `AppState` wired to the test pool, mirroring
/// `app.rs::Collaborators::new`. Cloud mode (`bootstrap_admin = false`).
/// Returns the `McpRefresher` alongside so the caller keeps its
/// coordinator task alive for the duration of the test.
#[allow(clippy::too_many_lines)]
async fn build_state(pool: PgPool) -> (AppState, patom::mcp::McpRefresher) {
    use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
    use patom::runtime::{
        PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget,
        SharedLeaseManager, SharedPromptQueue, SharedResponseSource, SharedThreadStream,
    };
    use patom::session::{PgSessionStore, SharedSessionStore};
    use tokio_util::sync::CancellationToken;

    let clock: SharedClock = SystemClock::shared();
    let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = queue_impl.clone();
    let leases: SharedLeaseManager = queue_impl;
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let responses: SharedResponseSource = hub;
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let agent_store = common::pg::shared_agent_store(pool.clone(), clock.clone());
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
    let state = AppState {
        queue,
        leases,
        responses,
        sessions,
        agents: agent_store,
        dag,
        budget: Arc::new(patom::budget::PgBudgetService::new(
            pool.clone(),
            SystemClock::shared(),
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
        orgs: Arc::new(PgOrgStore::new(pool.clone())),
        mailer: Arc::new(patom::orgs::LogMailer),
    };
    (state, refresher)
}

#[sqlx::test]
async fn accept_invite_endpoint_joins_and_switches_active_org(pool: PgPool) {
    let (state, _refresher) = build_state(pool.clone()).await;

    // The caller P already has their own workspace (seeded user + org +
    // owner membership) and a valid session cookie.
    let principal = common::auth::seed_principal(&pool, &state.jwt).await;

    // A separate admin A owns the inviting org O and mints an invite.
    let users = PgUserStore::new(pool.clone());
    let orgs = PgOrgStore::new(pool.clone());
    let now = chrono::Utc::now();
    let admin = users
        .upsert_from_oidc(
            &OidcProfile {
                issuer: IssuerUrl::try_from("https://idp.test").expect("issuer"),
                subject: OidcSubject::try_from("inviter").expect("subject"),
                email: Email::try_from("inviter@corp.test").expect("email"),
                email_verified: true,
                display_name: Some("Inviter".to_owned()),
                avatar_url: None,
                locale: None,
            },
            now,
        )
        .await
        .expect("admin")
        .user;
    let inviting_org = users
        .create_personal_org(admin.id, "corp", "Corp", Language::DEFAULT, now)
        .await
        .expect("inviting org");
    let invitee_email = Email::try_from("teammate@corp.test").expect("email");
    let token = orgs
        .create_invites(
            inviting_org.id,
            std::slice::from_ref(&invitee_email),
            Role::Member,
            admin.id,
            now,
            chrono::Duration::hours(48),
        )
        .await
        .expect("create invite")
        .remove(0)
        .token;

    // P redeems the token over HTTP.
    let app = router(state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/me/invites/accept")
                .header("cookie", principal.cookie_header())
                .header("x-csrf-token", principal.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "token": token.as_str() }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(
        res.status(),
        axum::http::StatusCode::OK,
        "redeeming a valid token succeeds"
    );
    // The response re-mints the session cookie so the switch takes hold.
    let set_cookie = res
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        set_cookie.contains(patom::auth::limits::COOKIE_NAME),
        "a fresh session cookie is set, got headers: {set_cookie}"
    );

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let expected_org = serde_json::to_value(inviting_org.id).expect("serialize org id");
    assert_eq!(
        json["active_org_id"], expected_org,
        "active org switches to the inviting org",
    );
    assert_eq!(json["role"].as_str(), Some("member"), "joins as member");

    // The membership is real: P is now a member of O.
    assert_eq!(
        users
            .membership(principal.user_id, inviting_org.id)
            .await
            .expect("membership"),
        Some(Role::Member),
    );
}
