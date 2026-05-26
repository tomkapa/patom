//! End-to-end probe for the asset upload endpoints.
//!
//! Covers the contract surface promised by CLAUDE.md §3:
//!   * Happy-path avatar upload writes both R2 and `users.avatar_url`.
//!   * Oversize bodies are rejected (413).
//!   * Magic-byte mismatch is rejected (400).
//!   * Built-in (global) catalog ids cannot be edited via upload (403).
//!   * Member-role principals cannot upload catalog icons (403).
//!   * `AppState.assets = None` yields 503 (asset storage not configured).
//!
//! The store is in-memory via `InMemoryAssetStore`; the integration test
//! never touches real R2.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use relay_rs::assets::{InMemoryAssetStore, SharedAssetStore};
use relay_rs::auth::OrgId;
use relay_rs::clock::SystemClock;
use relay_rs::http::{AppState, router};
use relay_rs::mcp::{
    McpRefresher, McpRegistry, PgMcpCatalogStore, PgMcpServerStore, SharedMcpCatalogStore,
    SharedMcpServerStore,
};
use relay_rs::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use relay_rs::session::{PgSessionStore, SharedSessionStore};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::TestDb;

const BOUNDARY: &str = "----relay-test-boundary";

/// Test helper — encode one multipart field carrying an image body.
fn build_multipart(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"upload.bin\"\r\n",
    );
    out.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// Minimal byte fixtures — only magic-byte prefixes need to match.
const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const JPEG_HEADER: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];

fn pad(prefix: &[u8], total: usize) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.resize(total, 0);
    v
}

struct UploadsHarness {
    state: AppState,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    mcp_catalog: SharedMcpCatalogStore,
    #[allow(dead_code)]
    refresher: McpRefresher,
    db: TestDb,
}

impl UploadsHarness {
    async fn new_with_assets(assets: Option<SharedAssetStore>) -> Self {
        let db = TestDb::fresh().await;
        let clock = SystemClock::shared();
        let pool: PgPool = db.pool.clone();

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
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: relay_rs::memory::SharedMemoryStore =
            Arc::new(relay_rs::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));

        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        let primary = seed_principal(&pool, &jwt).await;

        let mcp_catalog: SharedMcpCatalogStore = Arc::new(PgMcpCatalogStore::new(pool.clone()));

        let state = AppState {
            queue,
            leases,
            responses,
            sessions,
            agents,
            prompt_versions: Arc::new(
                relay_rs::agents::prompt_versions::PgPromptVersionStore::new(
                    pool.clone(),
                    clock.clone(),
                ),
            ),
            dag,
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog: mcp_catalog.clone(),
            mcp_refresh,
            mcp_credentials: Arc::new(relay_rs::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                Arc::new(relay_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: relay_rs::mcp::TestConnectRateLimiter::new(clock.clone()),
            mcp_oauth_clients: Arc::new(relay_rs::mcp::oauth::PgMcpOAuthClientStore::new(
                pool.clone(),
                clock.clone(),
                Arc::new(relay_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_oauth_pending: Arc::new(relay_rs::mcp::oauth::PgMcpOAuthPendingStore::new(
                pool.clone(),
                clock.clone(),
            )),
            mcp_oauth_flow: relay_rs::mcp::oauth::OAuthFlowClient::new(reqwest::Client::new())
                .expect("oauth http"),
            oauth_redirect_base: Arc::from("http://localhost:8080"),
            web_base_url: None,
            thread_stream,
            pool: pool.clone(),
            jwt,
            oauth,
            users,
            clock: clock.clone(),
            cookie_secure: false,
            memberships: Arc::new(relay_rs::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets,
        };

        Self {
            state,
            primary,
            mcp_catalog,
            refresher,
            db,
        }
    }

    /// Build a harness with a wired in-memory store and hand back the
    /// concrete store handle so tests can assert against it.
    async fn with_assets() -> (Self, Arc<InMemoryAssetStore>) {
        let inspect = Arc::new(InMemoryAssetStore::new("https://assets.test.invalid"));
        let shared: SharedAssetStore = inspect.clone();
        let h = Self::new_with_assets(Some(shared)).await;
        (h, inspect)
    }

    async fn without_assets() -> Self {
        Self::new_with_assets(None).await
    }

    /// Seed an org-scoped `mcp_catalog` row owned by `org`. Returns the
    /// catalog id string. The row defaults to `auth_kind = 'none'` so
    /// no upstream credentials are needed for the test.
    async fn seed_org_catalog(&self, org: OrgId, catalog_id: &str, display: &str) {
        sqlx::query(
            "INSERT INTO mcp_catalog \
                (id, org_id, display_name, description, default_transport, auth_kind) \
             VALUES ($1, $2, $3, $3, \
                 '{\"type\":\"http\",\"url\":\"https://example.test/mcp\"}'::jsonb, 'none')",
        )
        .bind(catalog_id)
        .bind(org.as_uuid())
        .bind(display)
        .execute(&self.db.pool)
        .await
        .expect("seed org-scoped catalog");
    }

    /// Demote the principal's role on their seeded org. Used by the
    /// member-rejection test path.
    async fn demote_to_member(&self, principal: &SeededPrincipal) {
        sqlx::query("UPDATE org_members SET role = 'member' WHERE org_id = $1 AND user_id = $2")
            .bind(principal.org_id)
            .bind(principal.user_id)
            .execute(&self.db.pool)
            .await
            .expect("demote role");
    }
}

fn upload_request(
    uri: &str,
    principal: &SeededPrincipal,
    body: Vec<u8>,
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("Cookie", principal.cookie_header())
        .header("X-CSRF-Token", principal.csrf_header())
        .body(axum::body::Body::from(body))
        .expect("request")
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_upload_happy_path_writes_store_and_db() {
    let (h, store) = UploadsHarness::with_assets().await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    let url = json["url"].as_str().expect("url field");
    let expected_key = format!("avatars/{}.png", h.primary.user_id);
    assert!(url.ends_with(&format!("/{expected_key}")), "url = {url}");
    assert_eq!(store.len().await, 1);

    let row: (Option<String>,) = sqlx::query_as("SELECT avatar_url FROM users WHERE id = $1")
        .bind(h.primary.user_id)
        .fetch_one(&h.db.pool)
        .await
        .expect("read user");
    assert_eq!(row.0.as_deref(), Some(url));
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_upload_oversize_returns_413() {
    let (h, store) = UploadsHarness::with_assets().await;
    let app = router(h.state.clone());
    // Just past the 2 MiB cap. The outer RequestBodyLimitLayer (4 MiB)
    // permits this, so the framework error path is `DefaultBodyLimit`
    // on the route, mapped to 413 via tower-http.
    let oversize = pad(PNG_HEADER, 2 * 1024 * 1024 + 1);
    let body = build_multipart("image/png", &oversize);
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_upload_magic_byte_mismatch_returns_400() {
    let (h, store) = UploadsHarness::with_assets().await;
    let app = router(h.state.clone());
    // PNG bytes, but the multipart field claims JPEG.
    let body = build_multipart("image/jpeg", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_upload_svg_rejected_for_avatars() {
    let (h, store) = UploadsHarness::with_assets().await;
    let app = router(h.state.clone());
    let body = build_multipart(
        "image/svg+xml",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    );
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_routes_503_when_assets_unconfigured() {
    let h = UploadsHarness::without_assets().await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_catalog_icon_upload_owner_writes_store_and_db() {
    let (h, store) = UploadsHarness::with_assets().await;
    h.seed_org_catalog(h.primary.org_id, "custom-tile", "Custom Tile")
        .await;
    let app = router(h.state.clone());
    let body = build_multipart("image/jpeg", &pad(JPEG_HEADER, 1024));
    let req = upload_request("/api/uploads/mcp-catalog/custom-tile", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    assert_eq!(store.len().await, 1);

    let row: (Option<String>,) =
        sqlx::query_as("SELECT icon_url FROM mcp_catalog WHERE id = $1 AND org_id = $2")
            .bind("custom-tile")
            .bind(h.primary.org_id.as_uuid())
            .fetch_one(&h.db.pool)
            .await
            .expect("read mcp_catalog");
    assert!(
        row.0
            .expect("icon_url set")
            .ends_with("/mcp/custom-tile.jpg")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_catalog_icon_upload_rejects_builtin_id() {
    let (h, store) = UploadsHarness::with_assets().await;
    // 'notion' is seeded as a built-in (org_id IS NULL) by migration 30.
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/mcp-catalog/notion", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_catalog_icon_upload_rejects_member_role() {
    let (h, store) = UploadsHarness::with_assets().await;
    h.seed_org_catalog(h.primary.org_id, "tile-for-member", "Tile")
        .await;
    h.demote_to_member(&h.primary).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/mcp-catalog/tile-for-member", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_catalog_icon_upload_unknown_id_returns_400() {
    let (h, store) = UploadsHarness::with_assets().await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/mcp-catalog/never-seeded", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    // McpError::CatalogIdUnknown → BadRequest (per http/error.rs).
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}
