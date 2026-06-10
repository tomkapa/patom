//! End-to-end probe for the tenancy foundation.
//!
//! Verifies the three contracts the task exit criterion calls out:
//!   1. Unauthenticated `GET /mcp-servers` → 401.
//!   2. Authenticated `GET /mcp-servers` for a new org → 200, `[]`.
//!   3. Cross-org isolation: a row inserted under org A is invisible to
//!      a request authenticated as org B.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{
    ClientSource, ConnectionStatus, McpAuthKind, McpCatalogId, McpHttpUrl, McpRefresher,
    McpRegistry, McpServerCreate, McpTransport, PgMcpServerStore, SharedMcpServerStore,
};
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

struct AuthMcpHarness {
    state: AppState,
    mcp_store: SharedMcpServerStore,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl AuthMcpHarness {
    async fn new(pool: &PgPool) -> Self {
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
        let primary = seed_principal(pool, &jwt).await;

        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(
                pool.clone(),
                ::std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            ));

        let state = AppState {
            queue,
            responses,
            agents,
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: std::sync::Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog,
            mcp_refresh,
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
            assets: None,
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
            entitlements: std::sync::Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            state,
            mcp_store,
            primary,
            refresher,
        }
    }

    async fn seed_mcp(
        &self,
        org: OrgId,
        created_by_user_id: patom::auth::UserId,
        catalog_id_str: &str,
    ) {
        // Each unique test catalog id needs a matching `mcp_catalog` row
        // (the validation trigger added by migration 31). Seed it as a
        // global default so the trigger passes regardless of which test
        // org calls in.
        sqlx::query(
            "INSERT INTO mcp_catalog \
                (id, org_id, display_name, description, default_transport, auth_kind) \
             VALUES ($1, NULL, $1, $1, '{\"type\":\"http\",\"url\":\"https://example.com/mcp\"}'::jsonb, 'none') \
             ON CONFLICT DO NOTHING",
        )
        .bind(catalog_id_str)
        .execute(&self.state.pool)
        .await
        .expect("seed mcp_catalog");
        let catalog_id = McpCatalogId::try_from(catalog_id_str).expect("valid catalog id");
        let config = McpTransport::Http {
            url: McpHttpUrl::try_from(&*format!("http://localhost:9000/{catalog_id_str}"))
                .expect("valid url"),
        };
        self.mcp_store
            .create(McpServerCreate {
                org_id: org,
                created_by_user_id,
                catalog_id,
                config,
                description: None,
                enabled: true,
                connection_status: ConnectionStatus::Ok,
            })
            .await
            .expect("seed mcp server");
    }
}

#[sqlx::test]
async fn unauthenticated_get_mcp_servers_returns_401(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn authenticated_new_user_sees_empty_list(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
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
    assert_eq!(json, serde_json::json!([]));
}

#[sqlx::test]
async fn list_mcp_servers_surfaces_creator_email(pool: PgPool) {
    // Regression: the tenant-scoped tx for `list_mcp_servers` runs as
    // `patom_app`, and migration 14 REVOKEs ALL on `users` from that role.
    // An earlier change tried to LEFT JOIN onto `users` from inside the
    // tenant-scoped SELECT, which raised "permission denied for table users"
    // and surfaced to the client as a 500 "auth error". Enrichment must
    // happen via the privileged user store after the tx commits.
    let h = AuthMcpHarness::new(&pool).await;
    h.seed_mcp(h.primary.org_id, h.primary.user_id, "with-creator")
        .await;

    let expected_email: String = sqlx::query_scalar("SELECT email::text FROM users WHERE id = $1")
        .bind(h.primary.user_id)
        .fetch_one(&h.state.pool)
        .await
        .expect("read seeded user email");

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
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
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["creator_email"].as_str(),
        Some(expected_email.as_str()),
    );
}

#[sqlx::test]
async fn cross_org_isolation_filters_to_caller_org(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;

    // Mint a *second* principal in a different org and seed one row in
    // each org via the privileged store.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    h.seed_mcp(h.primary.org_id, h.primary.user_id, "mine")
        .await;
    h.seed_mcp(other.org_id, other.user_id, "theirs").await;

    let app = router(h.state.clone());

    // Primary principal sees only their own org's row.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
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
    let aliases: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["catalog_id"].as_str().expect("catalog_id"))
        .collect();
    assert_eq!(aliases, vec!["mine"]);

    // The other principal sees only theirs.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
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
    let aliases: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["catalog_id"].as_str().expect("catalog_id"))
        .collect();
    assert_eq!(aliases, vec!["theirs"]);
}

#[sqlx::test]
async fn list_scoped_to_active_org_not_all_memberships(pool: PgPool) {
    // A user who belongs to two orgs must see only the *active* org's
    // servers. RLS gates on membership in *any* org, so the route has to
    // pin the active org explicitly.
    let h = AuthMcpHarness::new(&pool).await;
    let other_org = join_second_org(&h.state.pool, h.primary.user_id).await;
    h.seed_mcp(h.primary.org_id, h.primary.user_id, "active-srv")
        .await;
    h.seed_mcp(other_org, h.primary.user_id, "other-srv").await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
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
    let aliases: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["catalog_id"].as_str().expect("catalog_id"))
        .collect();
    assert_eq!(aliases, vec!["active-srv"]);
}

#[sqlx::test]
async fn read_by_id_404s_for_non_active_org_membership(pool: PgPool) {
    // `visible_to` must pin the active org: a server in another org the
    // user belongs to is not readable while that org is not active.
    let h = AuthMcpHarness::new(&pool).await;
    let other_org = join_second_org(&h.state.pool, h.primary.user_id).await;
    h.seed_mcp(other_org, h.primary.user_id, "other-srv").await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM mcp_servers WHERE catalog_id = 'other-srv' AND org_id = $1",
    )
    .bind(other_org)
    .fetch_one(&h.state.pool)
    .await
    .expect("read seeded server id");

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/api/mcp-servers/{id}"))
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn unauthenticated_test_connect_returns_401(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers/test-connect")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"config":{"type":"http","url":"http://localhost:1/"}}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_connect_against_dead_url_returns_failed_outcome(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers/test-connect")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                // 127.0.0.1:1 is the canonical "nothing listening" address.
                .body(axum::body::Body::from(
                    r#"{"config":{"type":"http","url":"http://127.0.0.1:1/"}}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(json["outcome"], "failed");
    assert!(!json["error"].as_str().expect("error string").is_empty());
    // No persisted side effect — the catalog stays empty.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers")
        .fetch_one(&h.state.pool)
        .await
        .expect("count");
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn test_connect_rate_limits_per_user(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let body = r#"{"config":{"type":"http","url":"http://127.0.0.1:1/"}}"#;
    let mut last_status = axum::http::StatusCode::OK;
    // Burst past the per-minute cap. `MCP_TEST_CONNECT_PER_MIN` is 10; sending
    // 12 calls back-to-back guarantees at least one 429.
    for _ in 0..12 {
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/mcp-servers/test-connect")
                    .header("cookie", h.primary.cookie_header())
                    .header("x-csrf-token", h.primary.csrf_header())
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response");
        last_status = res.status();
    }
    assert_eq!(last_status, axum::http::StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test]
async fn create_with_credentials_seals_to_encrypted_table(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    // Create a server *with* a secret-bearing header in one request.
    // Use a custom catalog id (not a built-in) so the full-form path
    // registers a fresh tenant-custom catalog row instead of being
    // rejected as shadow-global.
    let body = r#"{
        "catalog_id": "internal-secret-svc",
        "config": {"type": "http", "url": "http://127.0.0.1:1/"},
        "credentials": {
            "kind": "static_headers",
            "headers": {"authorization": "Bearer leaky-token-do-not-echo"}
        }
    }"#;
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    // R2: response never echoes the secret.
    let body_text = serde_json::to_string(&json).expect("ser");
    assert!(
        !body_text.contains("leaky-token-do-not-echo"),
        "response leaked the secret"
    );
    assert_eq!(json["has_credentials"], serde_json::json!(true));
    assert_eq!(
        json["credentials_kind"],
        serde_json::json!("static_headers")
    );

    // R2: DB ciphertext is opaque — the plaintext token never lives in the
    // table. Scope by id (rather than `LIMIT 1`) so the assertion stays
    // tight if future tests seed additional rows.
    let server_id: uuid::Uuid = json["id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .expect("create response had `id`");
    let cipher: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM mcp_server_credentials WHERE server_id = $1")
            .bind(server_id)
            .fetch_one(&h.state.pool)
            .await
            .expect("ciphertext");
    let cipher_str = String::from_utf8_lossy(&cipher);
    assert!(
        !cipher_str.contains("leaky-token-do-not-echo"),
        "ciphertext is not opaque"
    );

    // R2: GET also does not surface the secret.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let list_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let list_text = String::from_utf8(list_bytes.to_vec()).expect("utf8");
    assert!(
        !list_text.contains("leaky-token-do-not-echo"),
        "list response leaked the secret"
    );
    let arr: serde_json::Value = serde_json::from_str(&list_text).expect("json");
    let row = arr.as_array().expect("array")[0].clone();
    assert_eq!(row["has_credentials"], serde_json::json!(true));
    assert_eq!(row["credentials_kind"], serde_json::json!("static_headers"));
}

/// Helper: POST /api/mcp-servers with the given JSON body, returning
/// (status, parsed-body). Mirrors the inline pattern in the create tests.
async fn post_create(
    app: axum::Router,
    h: &AuthMcpHarness,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[sqlx::test]
async fn create_full_form_with_confidential_oauth_client_seals_on_catalog(pool: PgPool) {
    // Full-form custom URL + a confidential BYO-OAuth client (id + secret).
    // The auto-created catalog row is `user_supplied`/`oauth2`, the server
    // parks in `auth_pending`, and the secret round-trips out of the store
    // — but never appears in the HTTP response.
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let secret = "byo-oauth-secret-do-not-echo";
    let (status, json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "internal-oauth",
            "config": {"type": "http", "url": "https://internal.example.test/mcp"},
            "oauth_client": {"client_id": "byo-client-123", "client_secret": secret}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(json["connection_status"], serde_json::json!("auth_pending"));

    // Response is secret-free.
    let body_text = serde_json::to_string(&json).expect("ser");
    assert!(
        !body_text.contains(secret),
        "response leaked the client secret"
    );

    // Catalog row carries the discriminators.
    let id = McpCatalogId::try_from("internal-oauth").expect("valid id");
    let entry = h
        .state
        .mcp_catalog
        .get_for_org(h.primary.org_id, &id)
        .await
        .expect("get_for_org")
        .expect("catalog row exists");
    assert!(matches!(entry.client_source, ClientSource::UserSupplied));
    assert!(matches!(entry.auth_kind, McpAuthKind::OAuth2));

    // The dedicated decrypt seam returns the round-tripped client.
    let client = h
        .state
        .mcp_catalog
        .oauth_client(h.primary.org_id, &id)
        .await
        .expect("oauth_client")
        .expect("user-supplied client present");
    assert_eq!(client.client_id.as_str(), "byo-client-123");
    assert_eq!(
        client.client_secret.expect("secret present").expose(),
        secret
    );

    // DB ciphertext on the catalog row is opaque.
    let cipher: Vec<u8> = sqlx::query_scalar(
        "SELECT oauth_client_secret_ciphertext FROM mcp_catalog WHERE id = $1 AND org_id = $2",
    )
    .bind("internal-oauth")
    .bind(h.primary.org_id.as_uuid())
    .fetch_one(&h.state.pool)
    .await
    .expect("ciphertext");
    assert!(
        !String::from_utf8_lossy(&cipher).contains(secret),
        "catalog ciphertext is not opaque"
    );
}

#[sqlx::test]
async fn create_full_form_with_public_oauth_client_has_no_secret(pool: PgPool) {
    // Public/PKCE client: client_id only, no secret.
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let (status, json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "public-oauth",
            "config": {"type": "http", "url": "https://public.example.test/mcp"},
            "oauth_client": {"client_id": "public-client-xyz"}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(json["connection_status"], serde_json::json!("auth_pending"));

    let id = McpCatalogId::try_from("public-oauth").expect("valid id");
    let client = h
        .state
        .mcp_catalog
        .oauth_client(h.primary.org_id, &id)
        .await
        .expect("oauth_client")
        .expect("user-supplied client present");
    assert_eq!(client.client_id.as_str(), "public-client-xyz");
    assert!(
        client.client_secret.is_none(),
        "public client has no secret"
    );
}

#[sqlx::test]
async fn create_rejects_oauth_client_and_credentials_together(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let (status, _json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "both-auth",
            "config": {"type": "http", "url": "https://e.example.test/mcp"},
            "oauth_client": {"client_id": "cid"},
            "credentials": {"kind": "static_headers", "headers": {"authorization": "Bearer t"}}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn create_rejects_oauth_client_on_short_form(pool: PgPool) {
    // No `config` → short form. An OAuth client needs a custom server URL.
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let (status, _json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "gmail",
            "oauth_client": {"client_id": "cid"}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn create_rejects_oversized_oauth_client_fields(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());

    // Oversized client_id (cap is 512).
    let big_id = "a".repeat(513);
    let (status, _json) = post_create(
        app.clone(),
        &h,
        serde_json::json!({
            "catalog_id": "oversize-id",
            "config": {"type": "http", "url": "https://e.example.test/mcp"},
            "oauth_client": {"client_id": big_id}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    // Oversized client_secret (cap is 1024).
    let big_secret = "a".repeat(1025);
    let (status, _json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "oversize-secret",
            "config": {"type": "http", "url": "https://e.example.test/mcp"},
            "oauth_client": {"client_id": "cid", "client_secret": big_secret}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

/// The `client_id` the mock AS hands out from its Dynamic Client
/// Registration endpoint.
const MOCK_DCR_CLIENT_ID: &str = "dcr-registered-007";

/// Spawn a throwaway OAuth Authorization Server for the start-flow tests.
/// It answers the `oauth-authorization-server` discovery probe (so rmcp's
/// `discover_metadata()` resolves an `authorization_endpoint`) and the RFC
/// 7591 registration endpoint (so the DCR path can register a client).
/// Returns its base URL.
async fn spawn_mock_authorization_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock AS");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let meta_base = base.clone();
    let app = axum::Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(move || {
                let b = meta_base.clone();
                async move {
                    axum::Json(serde_json::json!({
                        "issuer": b,
                        "authorization_endpoint": format!("{b}/authorize"),
                        "token_endpoint": format!("{b}/token"),
                        "registration_endpoint": format!("{b}/register"),
                        "response_types_supported": ["code"],
                        "code_challenge_methods_supported": ["S256"],
                        "scopes_supported": ["read"],
                    }))
                }
            }),
        )
        .route(
            "/register",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    // Echo the requested redirect_uris (rmcp requires the field
                    // on the response) and hand back a fixed client_id.
                    let redirect_uris = body
                        .get("redirect_uris")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([]));
                    (
                        axum::http::StatusCode::CREATED,
                        axum::Json(serde_json::json!({
                            "client_id": MOCK_DCR_CLIENT_ID,
                            "redirect_uris": redirect_uris,
                        })),
                    )
                },
            ),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock AS");
    });
    base
}

#[sqlx::test]
async fn oauth_start_for_user_supplied_builds_authorize_url_with_supplied_client_id(pool: PgPool) {
    // The automated boundary the issue calls out: a user-supplied server's
    // `/oauth/start` returns an authorize URL whose `client_id` equals the
    // operator's own id. Real authorize→callback→token is a manual check.
    let h = AuthMcpHarness::new(&pool).await;
    let as_base = spawn_mock_authorization_server().await;

    // Create the custom OAuth server pointed at the mock AS.
    let app = router(h.state.clone());
    let (status, created) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "mock-oauth",
            "config": {"type": "http", "url": as_base},
            "oauth_client": {"client_id": "byo-client-123", "client_secret": "sekret"}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let server_id = created["id"].as_str().expect("server id");

    // Kick off the browser flow.
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/mcp-servers/{server_id}/oauth/start"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"scope": "read"}).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let authorize_url = json["authorize_url"].as_str().expect("authorize_url");

    // The authorize URL targets the mock AS's authorization_endpoint and
    // carries the operator's own client_id.
    let parsed = url::Url::parse(authorize_url).expect("valid authorize url");
    assert!(
        authorize_url.starts_with(&format!("{as_base}/authorize")),
        "authorize URL should target the discovered endpoint, got: {authorize_url}"
    );
    let client_id = parsed
        .query_pairs()
        .find(|(k, _)| k == "client_id")
        .map(|(_, v)| v.into_owned());
    assert_eq!(client_id.as_deref(), Some("byo-client-123"));
}

#[sqlx::test]
async fn create_full_form_oauth_via_dcr_needs_no_client(pool: PgPool) {
    // The common case: a custom OAuth server with no operator-supplied
    // client. The catalog row is `oauth2` / `dcr` (the server registers a
    // client dynamically), the server parks in `auth_pending`, and there is
    // no user-supplied client to read back.
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let (status, json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "dcr-oauth",
            "config": {"type": "http", "url": "https://dcr.example.test/mcp"},
            "oauth_client": {}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(json["connection_status"], serde_json::json!("auth_pending"));

    let id = McpCatalogId::try_from("dcr-oauth").expect("valid id");
    let entry = h
        .state
        .mcp_catalog
        .get_for_org(h.primary.org_id, &id)
        .await
        .expect("get_for_org")
        .expect("catalog row exists");
    assert!(matches!(entry.client_source, ClientSource::Dcr));
    assert!(matches!(entry.auth_kind, McpAuthKind::OAuth2));
    assert!(
        h.state
            .mcp_catalog
            .oauth_client(h.primary.org_id, &id)
            .await
            .expect("oauth_client")
            .is_none(),
        "a DCR connector carries no user-supplied client"
    );
}

#[sqlx::test]
async fn create_rejects_oauth_secret_without_client_id(pool: PgPool) {
    // A client_secret only applies to a pre-registered app — without a
    // client_id it is a misconfiguration, not a DCR request.
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let (status, _json) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "secret-no-id",
            "config": {"type": "http", "url": "https://e.example.test/mcp"},
            "oauth_client": {"client_secret": "orphan-secret"}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn oauth_start_for_dcr_registers_client_dynamically(pool: PgPool) {
    // URL-only OAuth: `/oauth/start` registers a client via the AS's RFC
    // 7591 endpoint and builds an authorize URL carrying that registered
    // client_id — the operator supplied no client.
    let h = AuthMcpHarness::new(&pool).await;
    let as_base = spawn_mock_authorization_server().await;

    let app = router(h.state.clone());
    let (status, created) = post_create(
        app,
        &h,
        serde_json::json!({
            "catalog_id": "mock-dcr",
            "config": {"type": "http", "url": as_base},
            "oauth_client": {}
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let server_id = created["id"].as_str().expect("server id");

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/mcp-servers/{server_id}/oauth/start"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"scope": "read"}).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let authorize_url = json["authorize_url"].as_str().expect("authorize_url");

    let parsed = url::Url::parse(authorize_url).expect("valid authorize url");
    let client_id = parsed
        .query_pairs()
        .find(|(k, _)| k == "client_id")
        .map(|(_, v)| v.into_owned());
    assert_eq!(client_id.as_deref(), Some(MOCK_DCR_CLIENT_ID));
}

#[sqlx::test]
async fn put_credentials_replaces_without_revealing_old_value(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    h.seed_mcp(h.primary.org_id, h.primary.user_id, "rotate")
        .await;
    let server_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM mcp_servers WHERE catalog_id = 'rotate' AND org_id = $1",
    )
    .bind(h.primary.org_id)
    .fetch_one(&h.state.pool)
    .await
    .expect("seeded id");

    let app = router(h.state.clone());
    // First write
    let first = r#"{"kind":"static_headers","headers":{"authorization":"Bearer old-secret"}}"#;
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/api/mcp-servers/{server_id}/credentials"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NO_CONTENT);
    let first_cipher: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM mcp_server_credentials WHERE server_id = $1")
            .bind(server_id)
            .fetch_one(&h.state.pool)
            .await
            .expect("first ciphertext");

    // Replace
    let second = r#"{"kind":"static_headers","headers":{"authorization":"Bearer new-secret"}}"#;
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/api/mcp-servers/{server_id}/credentials"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(second))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NO_CONTENT);
    let second_cipher: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM mcp_server_credentials WHERE server_id = $1")
            .bind(server_id)
            .fetch_one(&h.state.pool)
            .await
            .expect("second ciphertext");
    assert_ne!(first_cipher, second_cipher);
    let raw = String::from_utf8_lossy(&second_cipher);
    assert!(!raw.contains("old-secret"));
    assert!(!raw.contains("new-secret"));
}

/// `GET /mcp-oauth/callback` runs without a session cookie. When the
/// vendor reports an `error=…` query parameter (user denied consent,
/// scope rejected, …) the handler must redirect to the FE with
/// `?status=failed&reason=…` so the Failed frame can render — not
/// terminate the flow with a bare 400.
#[sqlx::test]
async fn oauth_callback_vendor_error_redirects_with_failed_status(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/mcp-oauth/callback?error=access_denied&error_description=user%20said%20no")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location header")
        .to_str()
        .expect("ascii location");
    assert_eq!(loc, "/?status=failed&reason=access_denied");
}

/// When `state` is missing the handler has nothing to consume and no
/// `redirect_to` to honour; it must still bounce the user to a
/// FE-friendly URL rather than 400.
#[sqlx::test]
async fn oauth_callback_missing_state_redirects_to_root_failed(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/mcp-oauth/callback?code=irrelevant")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location header")
        .to_str()
        .expect("ascii location");
    assert_eq!(loc, "/?status=failed&reason=state_missing");
}

/// An unknown `state` value (replay, expired) carries no pending row,
/// so we fall back to `/` for the redirect base. `reason=unknown_or_
/// expired_state` lets the FE distinguish replay attempts from
/// vendor-side errors.
#[sqlx::test]
async fn oauth_callback_unknown_state_redirects_to_root_failed(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/mcp-oauth/callback?code=irrelevant&state=never-issued")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location header")
        .to_str()
        .expect("ascii location");
    assert_eq!(loc, "/?status=failed&reason=unknown_or_expired_state");
}

/// Full-form `POST /mcp-servers` for a brand-new tenant-custom MCP:
/// the handler must auto-upsert the matching `mcp_catalog` row so the
/// FK trigger sees a valid parent. Also pins `connection_status` to
/// `"ok"` for a no-auth custom server — parking it in `auth_pending`
/// would cause the refresher to skip it forever, since there's no
/// OAuth callback to flip the bit.
#[sqlx::test]
async fn custom_create_auto_upserts_catalog_entry(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let app = router(h.state.clone());
    let body = serde_json::json!({
        "catalog_id": "pencil",
        "display_name": "Pencil",
        "config": { "type": "http", "url": "http://localhost:8000/sse" },
    });
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(created["connection_status"], serde_json::json!("ok"));

    let app = router(h.state.clone());
    let listed = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-catalog")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("catalog response");
    assert_eq!(listed.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let ids: Vec<&str> = json
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|row| row["catalog_id"].as_str())
        .collect();
    assert!(
        ids.contains(&"pencil"),
        "pencil missing from catalog: {ids:?}"
    );
}

/// Re-posting the same `(org, catalog_id)` for a custom server hits
/// the *server-row* uniqueness. Surfaces as `CatalogIdTaken` → 409.
/// The catalog row must NOT be mutated by the second call — that
/// would leak state on a request whose top-line response is a refusal.
#[sqlx::test]
async fn custom_create_second_call_is_conflict_and_does_not_mutate_catalog(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let first_body = serde_json::json!({
        "catalog_id": "pencil",
        "display_name": "Pencil",
        "config": { "type": "http", "url": "http://localhost:8000/sse" },
    });

    let first = router(h.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(first_body.to_string()))
                .expect("request"),
        )
        .await
        .expect("first");
    assert_eq!(first.status(), axum::http::StatusCode::CREATED);

    // Second POST: different display_name, different URL, *and* inline
    // static_headers credentials. None of these may bleed into the
    // existing catalog row.
    let second_body = serde_json::json!({
        "catalog_id": "pencil",
        "display_name": "Should Not Stick",
        "config": { "type": "http", "url": "http://attacker.example/" },
        "credentials": {
            "kind": "static_headers",
            "headers": { "X-Bad": "x" },
        },
    });
    let second = router(h.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(second_body.to_string()))
                .expect("request"),
        )
        .await
        .expect("second");
    assert_eq!(second.status(), axum::http::StatusCode::CONFLICT);

    // List the catalog and find the `pencil` row — its display_name
    // and auth_kind must still reflect the first POST, not the second.
    let listed = router(h.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/mcp-catalog")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("catalog response");
    assert_eq!(listed.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let pencil = json
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["catalog_id"] == "pencil")
        .expect("pencil row present");
    assert_eq!(
        pencil["display_name"], "Pencil",
        "second POST must not overwrite display_name",
    );
    assert_eq!(
        pencil["auth_kind"], "none",
        "second POST must not flip auth_kind to static_headers",
    );
}

/// A custom create that picks a catalog_id matching a built-in (e.g.
/// `notion`) is refused with 409, not silently shadowed. Prevents an
/// operator from breaking their built-in Notion wiring by accident.
#[sqlx::test]
async fn custom_create_rejects_shadowing_global_id(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let body = serde_json::json!({
        "catalog_id": "notion",
        "display_name": "Self-hosted Notion",
        "config": { "type": "http", "url": "http://localhost:9000/" },
    });
    let res = router(h.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::CONFLICT);
}

/// Short-form create for an OAuth catalog entry (e.g. `gmail`) plus
/// inline `static_headers` credentials must be refused with 400. A
/// server whose catalog auth_kind is OAuth2 cannot serve traffic from
/// static headers — the refresher would publish it as `Ok` forever
/// while every tool call 401s. The fix surfaces the mismatch up-front
/// instead of letting it land silently.
#[sqlx::test]
async fn oauth_catalog_with_inline_credentials_is_rejected(pool: PgPool) {
    let h = AuthMcpHarness::new(&pool).await;
    let body = serde_json::json!({
        "catalog_id": "gmail",
        "credentials": {
            "kind": "static_headers",
            "headers": { "Authorization": "Bearer not-an-oauth-token" },
        },
    });
    let res = router(h.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp-servers")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
}
