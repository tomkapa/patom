//! HTTP contract for user-created channels (`/api/channels`).
//!
//! Covers: the org-bootstrap `#general` seed trigger, member-scoped listing,
//! creator-only mutation (rename / archive / membership), the immutability of
//! the system `#general`, and that membership changes flip a channel's
//! visibility for another human in the same org.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::agents::{AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent};
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
use common::auth::{SeededPrincipal, TEST_CSRF_TOKEN, seed_principal};
use common::pg::seed_tenant;

/// Build a full `AppState` over the test pool, plus a fresh primary principal
/// in its own org. Mirrors the `auth_threads` harness.
async fn build(pool: &PgPool) -> (AppState, SeededPrincipal) {
    let _seed = seed_tenant(pool).await;
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
    let primary = seed_principal(pool, &jwt).await;

    let state = AppState {
        queue,
        responses,
        agents,
        colleagues: Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
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
    (state, primary)
}

/// Add a second human to `org` as a plain member and mint their cookie. The
/// `org_members` insert fires the migration-62 trigger that enrolls them into
/// `#general`.
async fn add_member_user(pool: &PgPool, jwt: &JwtSigner, org: OrgId) -> SeededPrincipal {
    let user_id = UserId::new();
    let email = format!("member-{}@example.test", &uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1,$2,'Member',$3,$3)")
        .bind(user_id).bind(&email).bind(now)
        .execute(pool).await.expect("seed user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1,$2,'member',$3)",
    )
    .bind(org)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed membership");
    SeededPrincipal {
        user_id,
        org_id: org,
        cookie_value: jwt.mint(user_id, Some(org)).expect("mint jwt"),
    }
}

/// Fire one request through the full router. Attaches the principal's cookie
/// and, for non-GET, the matching CSRF header. Returns `(status, json)`; the
/// body is `Value::Null` when empty.
async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    who: Option<&SeededPrincipal>,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(p) = who {
        builder = builder.header("cookie", p.cookie_header());
        if method != "GET" {
            builder = builder.header("x-csrf-token", TEST_CSRF_TOKEN);
        }
    }
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(axum::body::Body::from(v.to_string()))
            .expect("request"),
        None => builder.body(axum::body::Body::empty()).expect("request"),
    };
    let res = router(state.clone()).oneshot(req).await.expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response body is not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("array")
        .iter()
        .map(|c| c["name"].as_str().expect("name").to_owned())
        .collect()
}

/// Seed an agent in `org` so `POST /prompts` has a receiver. Returns its id as
/// a string for the request body.
async fn create_agent(state: &AppState, org: OrgId) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let record = state
        .agents
        .create(NewAgent {
            org_id: org,
            name: AgentName::try_from(format!("agent-{}", &suffix[..8])).expect("name"),
            system_prompt: AgentSystemPrompt::try_from("prompt").expect("prompt"),
            description: AgentDescription::try_from("an agent").expect("desc"),
            allowed_mcp_tools: AllowedMcpTools::empty(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("seed agent");
    record.id.as_uuid().to_string()
}

#[sqlx::test]
async fn channel_post_lands_in_channel_feed_not_dms(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let agent = create_agent(&state, primary.org_id).await;
    let (_, ch) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    let ch_id = ch["id"].as_str().expect("id").to_owned();

    let (status, _) = send(
        &state,
        "POST",
        "/api/prompts",
        Some(&primary),
        Some(json!({"content":"hello channel","idempotency_key":"k-ch","tags":[{"kind":"agent","id":agent}],"channel_id":ch_id})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);

    // Visible in the channel feed…
    let (status, feed) = send(
        &state,
        "GET",
        &format!("/api/threads?channel_id={ch_id}"),
        Some(&primary),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        feed.as_array().expect("array").len(),
        1,
        "channel feed shows the post"
    );

    // …but not in the DM feed (no channel_id param).
    let (_, dms) = send(&state, "GET", "/api/threads", Some(&primary), None).await;
    assert_eq!(
        dms.as_array().expect("array").len(),
        0,
        "channel post is not a DM"
    );
}

#[sqlx::test]
async fn dm_is_private_to_creator(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let other = add_member_user(&pool, &state.jwt, primary.org_id).await;
    let agent = create_agent(&state, primary.org_id).await;

    // No channel_id → a direct message.
    let (status, _) = send(
        &state,
        "POST",
        "/api/prompts",
        Some(&primary),
        Some(json!({"content":"private","idempotency_key":"k-dm","counterpart":{"kind":"agent","id":agent}})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);

    let (_, mine) = send(&state, "GET", "/api/threads", Some(&primary), None).await;
    assert_eq!(
        mine.as_array().expect("array").len(),
        1,
        "creator sees their DM"
    );

    let (_, theirs) = send(&state, "GET", "/api/threads", Some(&other), None).await;
    assert_eq!(
        theirs.as_array().expect("array").len(),
        0,
        "another org human cannot see the DM",
    );
}

#[sqlx::test]
async fn non_member_cannot_see_or_post_to_channel(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let other = add_member_user(&pool, &state.jwt, primary.org_id).await;
    let agent = create_agent(&state, primary.org_id).await;
    let (_, ch) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    let ch_id = ch["id"].as_str().expect("id").to_owned();
    let (seed_status, _) = send(
        &state,
        "POST",
        "/api/prompts",
        Some(&primary),
        Some(json!({"content":"hi","idempotency_key":"k-ch","tags":[{"kind":"agent","id":agent}],"channel_id":ch_id})),
    )
    .await;
    assert_eq!(
        seed_status,
        axum::http::StatusCode::ACCEPTED,
        "seed post enqueued"
    );

    // Non-member's view of the channel feed is empty.
    let (_, feed) = send(
        &state,
        "GET",
        &format!("/api/threads?channel_id={ch_id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(
        feed.as_array().expect("array").len(),
        0,
        "non-member sees nothing"
    );

    // …and a non-member cannot post into it.
    let (status, _) = send(
        &state,
        "POST",
        "/api/prompts",
        Some(&other),
        Some(json!({"content":"sneak","idempotency_key":"k-sneak","tags":[{"kind":"agent","id":agent}],"channel_id":ch_id})),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "non-member can't post"
    );
}

#[sqlx::test]
async fn unauthenticated_channels_returns_401(pool: PgPool) {
    let (state, _primary) = build(&pool).await;
    let (status, _) = send(&state, "GET", "/api/channels", None, None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn fresh_principal_is_enrolled_in_system_general(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let (status, list) = send(&state, "GET", "/api/channels", Some(&primary), None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        names(&list),
        vec!["general"],
        "org-seed trigger gives #general"
    );
    let general = &list.as_array().expect("array")[0];
    assert_eq!(general["system"], json!(true), "#general is system-owned");
    assert_eq!(
        general["can_manage"],
        json!(false),
        "nobody can manage #general"
    );
}

#[sqlx::test]
async fn create_lists_and_dedupes(pool: PgPool) {
    let (state, primary) = build(&pool).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(created["name"], json!("eng"));
    assert_eq!(created["can_manage"], json!(true));
    assert_eq!(created["system"], json!(false));

    let (_, list) = send(&state, "GET", "/api/channels", Some(&primary), None).await;
    assert_eq!(
        names(&list),
        vec!["general", "eng"],
        "general first, then eng"
    );

    // Duplicate active name → 409.
    let (status, _) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);

    // Invalid name (a space survives lowercasing) → 400.
    let (status, _) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"Bad Name"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn creator_renames_and_archives(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let (_, created) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // Rename.
    let (status, renamed) = send(
        &state,
        "PATCH",
        &format!("/api/channels/{id}"),
        Some(&primary),
        Some(json!({"name":"engineering"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(renamed["name"], json!("engineering"));

    // Archive → drops out of the active list.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/channels/{id}"),
        Some(&primary),
        Some(json!({"archived":true})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (_, list) = send(&state, "GET", "/api/channels", Some(&primary), None).await;
    assert_eq!(names(&list), vec!["general"], "archived channel hidden");
}

#[sqlx::test]
async fn system_general_is_immutable(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let (_, list) = send(&state, "GET", "/api/channels", Some(&primary), None).await;
    let general_id = list.as_array().expect("array")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/channels/{general_id}"),
        Some(&primary),
        Some(json!({"name":"renamed"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn non_creator_cannot_mutate(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let other = add_member_user(&pool, &state.jwt, primary.org_id).await;
    let (_, created) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // `other` is in the same org (so RLS lets them see the row) but is not the
    // creator → 403, not 404.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/channels/{id}"),
        Some(&other),
        Some(json!({"name":"hijacked"})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    let (status, _) = send(
        &state,
        "POST",
        &format!("/api/channels/{id}/members"),
        Some(&other),
        Some(json!({"user_id": other.user_id.as_uuid()})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn membership_flips_visibility(pool: PgPool) {
    let (state, primary) = build(&pool).await;
    let other = add_member_user(&pool, &state.jwt, primary.org_id).await;
    let (_, created) = send(
        &state,
        "POST",
        "/api/channels",
        Some(&primary),
        Some(json!({"name":"eng"})),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // Before being added, `other` sees only #general.
    let (_, before) = send(&state, "GET", "/api/channels", Some(&other), None).await;
    assert_eq!(names(&before), vec!["general"], "non-member can't see eng");

    // Creator adds `other`.
    let (status, _) = send(
        &state,
        "POST",
        &format!("/api/channels/{id}/members"),
        Some(&primary),
        Some(json!({"user_id": other.user_id.as_uuid()})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    let (_, after) = send(&state, "GET", "/api/channels", Some(&other), None).await;
    assert_eq!(names(&after), vec!["general", "eng"], "member now sees eng");

    // Roster lists both members.
    let (status, roster) = send(
        &state,
        "GET",
        &format!("/api/channels/{id}/members"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(roster.as_array().expect("array").len(), 2);
    // Every roster row is profile-enriched (display name; avatar nullable) so
    // the FE mention list / DM sidebar can render humans without a second
    // endpoint.
    for row in roster.as_array().expect("array") {
        assert!(
            row["display_name"].as_str().is_some(),
            "member row carries a display name: {row:?}"
        );
        assert!(row.get("avatar_url").is_some(), "avatar key present");
    }

    // Creator removes `other` → visibility revoked.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/channels/{id}/members/{}", other.user_id.as_uuid()),
        Some(&primary),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (_, revoked) = send(&state, "GET", "/api/channels", Some(&other), None).await;
    assert_eq!(names(&revoked), vec!["general"], "removed member loses eng");
}
