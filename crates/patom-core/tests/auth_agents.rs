//! End-to-end probe for the agents-tenancy retrofit.
//!
//! Verifies the same three contracts as `tests/auth_mcp_servers.rs`:
//!   1. Unauthenticated `GET /agents` → 401.
//!   2. Authenticated `GET /agents` for a fresh principal → 200; the
//!      principal sees the agents in their own org (seeded via the
//!      store) and nothing from other orgs.
//!   3. Cross-org isolation: an agent inserted under org A is invisible
//!      to a request authenticated as org B and vice versa.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::agents::{
    AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent, PRESET_AVATAR_COUNT,
    SharedAgentStore,
};
use patom::assets::InMemoryAssetStore;
use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, join_second_org, seed_principal};
use common::pg::seed_tenant;

struct AuthAgentsHarness {
    state: AppState,
    agents: SharedAgentStore,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl AuthAgentsHarness {
    async fn new(pool: &PgPool) -> Self {
        let _seed = seed_tenant(pool).await;
        let clock = SystemClock::shared();

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
        // Fresh principal in its *own* org — distinct from the seeded
        // `_seed` org (which already has the `test-default`
        // agent). The primary principal's org has no agents yet, which
        // is the right baseline for the empty-list assertion below.
        let primary = seed_principal(pool, &jwt).await;

        let state = AppState {
            queue,
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
            discord: None,
            assets: None,
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
            entitlements: std::sync::Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            state,
            agents,
            primary,
            refresher,
        }
    }

    async fn seed_agent(&self, org: OrgId, name: &str) {
        self.agents
            .create(NewAgent {
                org_id: org,
                name: AgentName::try_from(name).expect("valid name"),
                system_prompt: AgentSystemPrompt::try_from("scoped test prompt")
                    .expect("valid prompt"),
                description: AgentDescription::try_from(format!("agent {name}"))
                    .expect("valid desc"),
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("seed agent");
    }
}

#[sqlx::test]
async fn unauthenticated_get_agents_returns_401(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn authenticated_new_user_sees_empty_list(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    // The primary principal's org is fresh; the seeded `test-default`
    // belongs to `_seed.org_id`, which the primary is not a member
    // of — RLS filters it out.
    assert_eq!(json, serde_json::json!([]));
}

#[sqlx::test]
async fn cross_org_isolation_filters_to_caller_org(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;

    // Second principal in a different org.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    h.seed_agent(h.primary.org_id, "alpha").await;
    h.seed_agent(other.org_id, "beta").await;

    let app = router(h.state.clone());

    // Primary principal sees only their own org's row.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let names: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["alpha"]);

    // The other principal sees only theirs.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", other.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let names: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["beta"]);
}

#[sqlx::test]
async fn list_includes_colleague_id(pool: PgPool) {
    // The roster row carries the agent's colleague id (the `colleagues` PK,
    // distinct from the agent id) so the FE can resolve an agent's
    // `{kind:"colleague", id}` send_message receiver to a name — see
    // web/src/lib/foldHistory.ts. Without it the recipient tag is missing on
    // the first message to an agent that has not yet posted in the thread.
    let h = AuthAgentsHarness::new(&pool).await;
    h.seed_agent(h.primary.org_id, "alpha").await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let row = &json.as_array().expect("array")[0];

    let agent_id = row["id"].as_str().expect("agent id");
    let colleague_id = row["colleague_id"]
        .as_str()
        .expect("roster row carries colleague_id");
    assert_ne!(
        colleague_id, agent_id,
        "colleague_id is the colleagues PK, not the agent id",
    );
    assert!(
        uuid::Uuid::parse_str(colleague_id).is_ok(),
        "colleague_id is a uuid: {colleague_id}",
    );
}

#[sqlx::test]
async fn list_scoped_to_active_org_not_all_memberships(pool: PgPool) {
    // A single user who belongs to *two* orgs must see only the agents
    // of the org their session is active in — RLS alone (membership in
    // *any* org) is not enough; the route must pin the active org. This
    // is the duplicate-recruiter-in-the-sidebar bug: an invited user who
    // also owns a personal workspace saw both orgs' default agents.
    let h = AuthAgentsHarness::new(&pool).await;

    // The primary principal's session is active in `primary.org_id`.
    // Make them a member of a *second* org as well.
    let other_org = join_second_org(&h.state.pool, h.primary.user_id).await;

    // One agent in the active org, one in the other org the user belongs to.
    h.seed_agent(h.primary.org_id, "active-agent").await;
    h.seed_agent(other_org, "other-agent").await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let names: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["active-agent"],
        "only the active org's agent is listed, not the other membership's",
    );
}

/// Issue #43: `POST /agents` accepts `avatar_url` and echoes it in the
/// response. Returns the created agent's JSON for callers that need the id.
async fn create_agent_with_avatar(
    app: &axum::Router,
    h: &AuthAgentsHarness,
    name: &str,
    avatar_url: &str,
) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": name,
                        "system_prompt": "be helpful",
                        "description": "an agent with a face",
                        "avatar_url": avatar_url,
                    }))
                    .unwrap(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[sqlx::test]
async fn agent_avatar_url_echoed_on_create(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let created =
        create_agent_with_avatar(&app, &h, "avatar-agent", "https://cdn.example/face.png").await;
    assert_eq!(
        created["avatar_url"].as_str(),
        Some("https://cdn.example/face.png"),
        "create echoes the avatar_url",
    );
}

#[sqlx::test]
async fn agent_avatar_url_cleared_by_null_patch(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let created =
        create_agent_with_avatar(&app, &h, "avatar-agent", "https://cdn.example/face.png").await;
    let id = created["id"].as_str().expect("id");

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/api/agents/{id}"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&serde_json::json!({ "avatar_url": null })).unwrap(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let updated: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        updated["avatar_url"].is_null(),
        "null PATCH clears the avatar_url",
    );
}

#[sqlx::test]
async fn create_rejects_malformed_avatar_url(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "bad-avatar",
                        "system_prompt": "be helpful",
                        "description": "rejected at the boundary",
                        "avatar_url": "not-a-url",
                    }))
                    .unwrap(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
}

/// Issue #43: the DB CHECK only caps `avatar_url` length, so a row that
/// bypasses the app (direct SQL, a bad migration) could hold a non-URL
/// string. `GET /agents/{id}` must not leak it — `into_response` re-parses
/// through `AvatarUrl` and drops the invalid value.
#[sqlx::test]
async fn read_drops_avatar_url_that_violates_the_url_invariant(pool: PgPool) {
    let h = AuthAgentsHarness::new(&pool).await;
    let app = router(h.state.clone());
    let created =
        create_agent_with_avatar(&app, &h, "avatar-agent", "https://cdn.example/face.png").await;
    let id = created["id"].as_str().expect("id");

    // Smuggle a length-valid but non-URL value straight into the column,
    // bypassing the `AvatarUrl` parse every app write path enforces.
    sqlx::query("UPDATE agents SET avatar_url = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).expect("uuid"))
        .bind("not a url")
        .execute(&h.state.pool)
        .await
        .expect("smuggle malformed avatar");

    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/agents/{id}"))
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let read: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        read["avatar_url"].is_null(),
        "a row violating the URL invariant is dropped, not leaked",
    );
}

/// POST `/api/agents` with the given JSON body, returning the response
/// status and parsed JSON. Used by the default-avatar tests below.
async fn post_agent(
    app: &axum::Router,
    h: &AuthAgentsHarness,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Swap in an in-memory asset store so the default-avatar paths can build
/// `{origin}/agents/agent-{n}.png` (the production origin comes from
/// `PATOM_S3_PUBLIC_HOST`).
fn with_assets(h: &mut AuthAgentsHarness, origin: &str) {
    h.state.assets = Some(Arc::new(InMemoryAssetStore::new(origin)));
}

#[sqlx::test]
async fn missing_avatar_assigns_random_default(pool: PgPool) {
    // A plain create with no avatar_url gets a random bundled avatar in
    // `1..=PRESET_AVATAR_COUNT` when the asset CDN is configured.
    let mut h = AuthAgentsHarness::new(&pool).await;
    with_assets(&mut h, "https://cdn.test");
    let app = router(h.state.clone());
    let (status, created) = post_agent(
        &app,
        &h,
        serde_json::json!({
            "name": "fresh-hire",
            "system_prompt": "be helpful",
            "description": "a brand new agent",
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let url = created["avatar_url"]
        .as_str()
        .expect("random default avatar");
    let n: u8 = url
        .strip_prefix("https://cdn.test/agents/agent-")
        .and_then(|s| s.strip_suffix(".png"))
        .expect("matches the bundled avatar pattern")
        .parse()
        .expect("numeric index");
    assert!(n >= 1, "index below 1: {n}");
    assert!(n <= PRESET_AVATAR_COUNT, "index above cap: {n}");
}

#[sqlx::test]
async fn explicit_avatar_url_not_overridden_by_default(pool: PgPool) {
    // An explicit `avatar_url` (e.g. an onboarding preset's hardcoded CDN
    // URL, or a user-set avatar) is kept as-is — the random default only
    // fills in when no avatar_url is sent.
    let mut h = AuthAgentsHarness::new(&pool).await;
    with_assets(&mut h, "https://cdn.test");
    let app = router(h.state.clone());
    let (status, created) = post_agent(
        &app,
        &h,
        serde_json::json!({
            "name": "explicit-face",
            "system_prompt": "be helpful",
            "description": "explicit url is preserved",
            "avatar_url": "https://asset.patom.app/agents/agent-2.png",
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(
        created["avatar_url"].as_str(),
        Some("https://asset.patom.app/agents/agent-2.png"),
    );
}
