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

use patom::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent,
};
use patom::assets::{InMemoryAssetStore, SharedAssetStore};
use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::http::{AppState, router};
use patom::mcp::{
    McpRefresher, McpRegistry, PgMcpCatalogStore, PgMcpServerStore, SharedMcpCatalogStore,
    SharedMcpServerStore,
};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::seed_tenant;

const BOUNDARY: &str = "----patom-test-boundary";

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
    pool: PgPool,
    #[allow(dead_code)]
    mcp_catalog: SharedMcpCatalogStore,
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl UploadsHarness {
    async fn new_with_assets(pool: PgPool, assets: Option<SharedAssetStore>) -> Self {
        let _seed = seed_tenant(&pool).await;
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
        let primary = seed_principal(&pool, &jwt).await;

        let mcp_catalog: SharedMcpCatalogStore = Arc::new(PgMcpCatalogStore::new(
            pool.clone(),
            ::std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
        ));

        let state = AppState {
            queue,
            responses,
            agents,
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            billing: std::sync::Arc::new(patom::billing::PgBillingService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog: mcp_catalog.clone(),
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
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
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
            lark: None,
            assets,
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
            entitlements: std::sync::Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            state,
            primary,
            pool,
            mcp_catalog,
            refresher,
        }
    }

    /// Build a harness with a wired in-memory store and hand back the
    /// concrete store handle so tests can assert against it.
    async fn with_assets(pool: PgPool) -> (Self, Arc<InMemoryAssetStore>) {
        let inspect = Arc::new(InMemoryAssetStore::new("https://assets.test.invalid"));
        let shared: SharedAssetStore = inspect.clone();
        let h = Self::new_with_assets(pool, Some(shared)).await;
        (h, inspect)
    }

    async fn without_assets(pool: PgPool) -> Self {
        Self::new_with_assets(pool, None).await
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
        .execute(&self.pool)
        .await
        .expect("seed org-scoped catalog");
    }

    /// Create an agent in `org` and return its id. Used by the
    /// agent-avatar upload tests (issue #43).
    async fn seed_agent(&self, org: OrgId, name: &str) -> AgentId {
        self.state
            .agents
            .create(NewAgent {
                org_id: org,
                name: AgentName::try_from(name).expect("valid name"),
                system_prompt: AgentSystemPrompt::try_from("be helpful").expect("valid prompt"),
                description: AgentDescription::try_from(format!("agent {name}"))
                    .expect("valid desc"),
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("seed agent")
            .id
    }

    /// Demote the principal's role on their seeded org. Used by the
    /// member-rejection test path.
    async fn demote_to_member(&self, principal: &SeededPrincipal) {
        sqlx::query("UPDATE org_members SET role = 'member' WHERE org_id = $1 AND user_id = $2")
            .bind(principal.org_id)
            .bind(principal.user_id)
            .execute(&self.pool)
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

#[sqlx::test]
async fn avatar_upload_happy_path_writes_store_and_db(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
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
        .fetch_one(&h.pool)
        .await
        .expect("read user");
    assert_eq!(row.0.as_deref(), Some(url));
}

#[sqlx::test]
async fn avatar_upload_oversize_returns_413(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
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

#[sqlx::test]
async fn avatar_upload_magic_byte_mismatch_returns_400(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    // PNG bytes, but the multipart field claims JPEG.
    let body = build_multipart("image/jpeg", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn avatar_upload_svg_rejected_for_avatars(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
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

#[sqlx::test]
async fn upload_routes_503_when_assets_unconfigured(pool: PgPool) {
    let h = UploadsHarness::without_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[sqlx::test]
async fn mcp_catalog_icon_upload_owner_writes_store_and_db(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
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
            .fetch_one(&h.pool)
            .await
            .expect("read mcp_catalog");
    assert!(
        row.0
            .expect("icon_url set")
            .ends_with("/mcp/custom-tile.jpg")
    );
}

#[sqlx::test]
async fn mcp_catalog_icon_upload_rejects_builtin_id(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    // 'notion' is seeded as a built-in (org_id IS NULL) by migration 30.
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/mcp-catalog/notion", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn mcp_catalog_icon_upload_rejects_member_role(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
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

#[sqlx::test]
async fn mcp_catalog_icon_upload_unknown_id_returns_400(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/mcp-catalog/never-seeded", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    // McpError::CatalogIdUnknown → BadRequest (per http/error.rs).
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn workspace_avatar_upload_writes_store_and_db(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/workspace-avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    let url = json["url"].as_str().expect("url field");
    let expected_key = format!("workspaces/{}.png", h.primary.org_id);
    assert!(url.ends_with(&format!("/{expected_key}")), "url = {url}");
    assert_eq!(store.len().await, 1);

    let row: (Option<String>,) =
        sqlx::query_as("SELECT avatar_url FROM organizations WHERE id = $1")
            .bind(h.primary.org_id)
            .fetch_one(&h.pool)
            .await
            .expect("read org");
    assert_eq!(row.0.as_deref(), Some(url));
}

#[sqlx::test]
async fn workspace_avatar_upload_rejects_member_role(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    h.demote_to_member(&h.primary).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/workspace-avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn workspace_avatar_upload_svg_rejected(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart(
        "image/svg+xml",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    );
    let req = upload_request("/api/uploads/workspace-avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn workspace_avatar_upload_503_when_assets_unconfigured(pool: PgPool) {
    let h = UploadsHarness::without_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request("/api/uploads/workspace-avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[sqlx::test]
async fn workspace_avatar_upload_oversize_returns_413(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    let oversize = pad(PNG_HEADER, 2 * 1024 * 1024 + 1);
    let body = build_multipart("image/png", &oversize);
    let req = upload_request("/api/uploads/workspace-avatar", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn agent_avatar_upload_writes_store_keyed_by_agent(pool: PgPool) {
    // Issue #43: uploading for an agent the caller's org owns stores the
    // object under `agents/<agent_id>.<ext>` and returns its URL. No DB
    // write here — the agent settings form persists on the next PUT.
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let agent_id = h.seed_agent(h.primary.org_id, "atlas").await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request(
        &format!("/api/uploads/agent-avatar/{}", agent_id.as_uuid()),
        &h.primary,
        body,
    );
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    let url = json["url"].as_str().expect("url field");
    let expected_key = format!("agents/{}.png", agent_id.as_uuid());
    assert!(url.ends_with(&format!("/{expected_key}")), "url = {url}");
    assert_eq!(store.len().await, 1);

    // Contract: the upload stores the object only — it must NOT persist to
    // `agents.avatar_url`. That happens on the subsequent `PUT /agents/{id}`.
    let row: (Option<String>,) = sqlx::query_as("SELECT avatar_url FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_one(&h.pool)
        .await
        .expect("read agent");
    assert_eq!(row.0, None, "upload must not persist avatar_url directly");
}

#[sqlx::test]
async fn agent_avatar_upload_cross_org_agent_returns_404(pool: PgPool) {
    // An agent in another org is invisible (RLS) — the upload 404s
    // without writing an object under that org's agent key.
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let other = seed_principal(&h.pool, &h.state.jwt).await;
    let foreign_agent = h.seed_agent(other.org_id, "foreign").await;
    let app = router(h.state.clone());
    let body = build_multipart("image/png", &pad(PNG_HEADER, 256));
    let req = upload_request(
        &format!("/api/uploads/agent-avatar/{}", foreign_agent.as_uuid()),
        &h.primary,
        body,
    );
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn agent_avatar_upload_svg_rejected(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let agent_id = h.seed_agent(h.primary.org_id, "svg-agent").await;
    let app = router(h.state.clone());
    let body = build_multipart(
        "image/svg+xml",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    );
    let req = upload_request(
        &format!("/api/uploads/agent-avatar/{}", agent_id.as_uuid()),
        &h.primary,
        body,
    );
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

// ---- Message attachments (issue #187) ----

const PDF_HEADER: &[u8] = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3";

#[sqlx::test]
async fn attachment_upload_pdf_returns_reference(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    let body = build_multipart("application/pdf", &pad(PDF_HEADER, 512));
    let req = upload_request("/api/uploads/attachment", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    assert_eq!(json["mime"], "application/pdf");
    assert_eq!(json["filename"], "upload.bin");
    assert_eq!(json["size"], 512);
    let url = json["url"].as_str().expect("url field");
    assert!(url.contains("/attachments/"), "url = {url}");
    assert!(url.ends_with(".pdf"), "url = {url}");
    assert_eq!(store.len().await, 1);
}

#[sqlx::test]
async fn attachment_upload_rejects_disallowed_type(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    // text/plain is not in the attachment allow-list.
    let body = build_multipart("text/plain", b"just some text");
    let req = upload_request("/api/uploads/attachment", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}

#[sqlx::test]
async fn attachment_upload_magic_byte_mismatch_returns_400(pool: PgPool) {
    let (h, store) = UploadsHarness::with_assets(pool).await;
    let app = router(h.state.clone());
    // Claims PDF but the bytes are not a PDF container.
    let body = build_multipart("application/pdf", b"MZ\x90\x00 not a pdf at all");
    let req = upload_request("/api/uploads/attachment", &h.primary, body);
    let res = app.oneshot(req).await.expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(store.is_empty().await);
}
