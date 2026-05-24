//! Trait-contract tests for [`relay_rs::mcp::PgMcpCatalogStore`].
//!
//! The catalog store's writes were added to support the "Custom URL"
//! flow in the connections UI — operators now create tenant-custom
//! catalog rows from the same `POST /mcp-servers` call that wires the
//! server. These tests pin the upsert contract: insert path, update
//! path, RLS isolation between orgs, and the rule that a tenant-custom
//! id may not collide with a built-in (no shadow-global).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use chrono::Utc;
use relay_rs::auth::{OrgId, UserId};
use relay_rs::clock::SystemClock;
use relay_rs::mcp::{
    CatalogUpsert, McpAuthKind, McpCatalogDescription, McpCatalogDisplayName, McpCatalogId,
    McpCatalogStore, McpError, McpHttpUrl, McpTransport, PgMcpCatalogStore,
};
use uuid::Uuid;

mod common;
use common::pg::TestDb;

fn store(db: &TestDb) -> Arc<PgMcpCatalogStore> {
    let _ = SystemClock::shared();
    Arc::new(PgMcpCatalogStore::new(db.pool.clone()))
}

fn cat(s: &str) -> McpCatalogId {
    McpCatalogId::try_from(s).expect("valid catalog id")
}

fn name(s: &str) -> McpCatalogDisplayName {
    McpCatalogDisplayName::try_from(s).expect("valid display name")
}

fn description(s: &str) -> McpCatalogDescription {
    McpCatalogDescription::try_from(s).expect("valid description")
}

fn http_transport(url: &str) -> McpTransport {
    McpTransport::Http {
        url: McpHttpUrl::try_from(url).expect("valid url"),
    }
}

/// Seed a second organisation in the test schema so isolation tests
/// have somewhere to put a rival row.
async fn seed_second_org(db: &TestDb) -> OrgId {
    let org_id = OrgId::new();
    let user_id = UserId::new();
    let now = Utc::now();
    let user_email = format!("rival-{}@example.test", Uuid::new_v4().simple());
    let org_slug = format!("rival-{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $4)",
    )
    .bind(user_id)
    .bind(&user_email)
    .bind("Rival User")
    .bind(now)
    .execute(&db.pool)
    .await
    .expect("seed rival user");
    sqlx::query(
        "INSERT INTO organizations \
            (id, name, slug, default_language, created_at, updated_at) \
         VALUES ($1, $2, $3, 'en', $4, $4)",
    )
    .bind(org_id)
    .bind("Rival Org")
    .bind(&org_slug)
    .bind(now)
    .execute(&db.pool)
    .await
    .expect("seed rival org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) \
         VALUES ($1, $2, 'owner', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(&db.pool)
    .await
    .expect("seed rival membership");
    org_id
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_inserts_new_org_scoped_row() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let now = Utc::now();

    let stored = store
        .ensure_org_scoped(CatalogUpsert {
            org_id: db.default_org_id,
            id: &cat("pencil"),
            display_name: &name("Pencil"),
            description: &description("Local Pencil MCP via SSE bridge."),
            default_transport: &http_transport("http://localhost:8000/sse"),
            auth_kind: McpAuthKind::None,
            now,
        })
        .await
        .expect("ensure insert");
    assert!(matches!(stored, McpAuthKind::None));

    let row = store
        .get_for_org(db.default_org_id, &cat("pencil"))
        .await
        .expect("get_for_org")
        .expect("row exists");
    assert_eq!(row.id.as_str(), "pencil");
    assert_eq!(row.org_id, Some(db.default_org_id));
    assert_eq!(row.display_name.as_str(), "Pencil");
    assert_eq!(row.description.as_str(), "Local Pencil MCP via SSE bridge.");
    assert!(matches!(row.auth_kind, McpAuthKind::None));
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_does_not_mutate_existing_row() {
    // Regression: a second `ensure_org_scoped` call with different
    // metadata must leave the original row untouched and return the
    // *stored* auth_kind, not the requested one. The earlier
    // upsert-with-DO-UPDATE behaviour silently mutated catalog rows
    // even when the caller's parallel mcp_servers insert was about to
    // 409 — a failed request leaking state.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let t0 = Utc::now();
    let id = cat("pencil");

    let first_kind = store
        .ensure_org_scoped(CatalogUpsert {
            org_id: db.default_org_id,
            id: &id,
            display_name: &name("Pencil"),
            description: &description("first"),
            default_transport: &http_transport("http://localhost:8000/sse"),
            auth_kind: McpAuthKind::None,
            now: t0,
        })
        .await
        .expect("first ensure");
    assert!(matches!(first_kind, McpAuthKind::None));

    let t1 = t0 + chrono::Duration::seconds(1);
    let second_kind = store
        .ensure_org_scoped(CatalogUpsert {
            org_id: db.default_org_id,
            id: &id,
            display_name: &name("Pencil Local"),
            description: &description("second"),
            default_transport: &http_transport("http://127.0.0.1:8080/sse"),
            auth_kind: McpAuthKind::StaticHeaders,
            now: t1,
        })
        .await
        .expect("second ensure");
    // The stored auth_kind wins — the second caller's `StaticHeaders`
    // request did NOT silently replace the original `None`.
    assert!(matches!(second_kind, McpAuthKind::None));

    let row = store
        .get_for_org(db.default_org_id, &id)
        .await
        .expect("get_for_org")
        .expect("row exists");
    assert_eq!(row.display_name.as_str(), "Pencil");
    assert_eq!(row.description.as_str(), "first");
    assert!(matches!(row.auth_kind, McpAuthKind::None));
    let McpTransport::Http { url } = row.default_transport;
    assert_eq!(url.as_str(), "http://localhost:8000/sse");
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_rejects_collision_with_global_id() {
    // `notion` is seeded as a global row by migration 30. A tenant
    // attempting to register their own `notion` must fail with
    // `CatalogIdShadowsGlobal` instead of silently shadowing the
    // built-in.
    let db = TestDb::fresh().await;
    let store = store(&db);

    let err = store
        .ensure_org_scoped(CatalogUpsert {
            org_id: db.default_org_id,
            id: &cat("notion"),
            display_name: &name("Self-hosted Notion"),
            description: &description("Our mirror."),
            default_transport: &http_transport("http://localhost:9000/"),
            auth_kind: McpAuthKind::None,
            now: Utc::now(),
        })
        .await
        .expect_err("must reject shadow-global");
    assert!(matches!(
        err,
        McpError::CatalogIdShadowsGlobal(ref id) if id.as_str() == "notion"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_isolates_orgs() {
    // Same `id` for two different orgs is allowed (each gets a
    // tenant-custom row); neither leaks into the other's view.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let other_org = seed_second_org(&db).await;
    let id = cat("internal-search");
    let now = Utc::now();

    store
        .ensure_org_scoped(CatalogUpsert {
            org_id: db.default_org_id,
            id: &id,
            display_name: &name("Default Org Search"),
            description: &description("default"),
            default_transport: &http_transport("http://localhost:9000/"),
            auth_kind: McpAuthKind::None,
            now,
        })
        .await
        .expect("default ensure");
    store
        .ensure_org_scoped(CatalogUpsert {
            org_id: other_org,
            id: &id,
            display_name: &name("Rival Org Search"),
            description: &description("rival"),
            default_transport: &http_transport("http://localhost:9001/"),
            auth_kind: McpAuthKind::None,
            now,
        })
        .await
        .expect("rival ensure");

    let from_default = store
        .get_for_org(db.default_org_id, &id)
        .await
        .expect("get_for_org default")
        .expect("row exists");
    let from_rival = store
        .get_for_org(other_org, &id)
        .await
        .expect("get_for_org rival")
        .expect("row exists");
    assert_eq!(from_default.display_name.as_str(), "Default Org Search");
    assert_eq!(from_rival.display_name.as_str(), "Rival Org Search");
    assert_eq!(from_default.org_id, Some(db.default_org_id));
    assert_eq!(from_rival.org_id, Some(other_org));
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_gmail_entry_carries_default_scope() {
    // Migration 38 seeds Gmail's default scope set on the built-in
    // catalog row. start_oauth falls back to this when the FE's POST
    // body doesn't override — without it Google rejects the authorize
    // request with `Missing required parameter: scope`.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let gmail = store
        .get_for_org(db.default_org_id, &cat("gmail"))
        .await
        .expect("get_for_org gmail")
        .expect("gmail row exists");
    let scope = gmail.default_scope.expect("gmail default_scope set");
    assert!(
        scope.contains("https://www.googleapis.com/auth/gmail.readonly"),
        "gmail default_scope must include gmail.readonly, got: {scope}"
    );
    assert!(
        scope.contains("https://www.googleapis.com/auth/gmail.compose"),
        "gmail default_scope must include gmail.compose, got: {scope}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_gcal_entry_carries_default_scope() {
    // Calendar mirror of the Gmail check — exercises the same migration
    // 38 path on the second seeded entry.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let gcal = store
        .get_for_org(db.default_org_id, &cat("gcal"))
        .await
        .expect("get_for_org gcal")
        .expect("gcal row exists");
    let scope = gcal.default_scope.expect("gcal default_scope set");
    assert!(
        scope.contains("https://www.googleapis.com/auth/calendar.events"),
        "gcal default_scope must include calendar.events, got: {scope}"
    );
    assert!(
        scope.contains("https://www.googleapis.com/auth/calendar.readonly"),
        "gcal default_scope must include calendar.readonly, got: {scope}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_notion_entry_has_no_default_scope() {
    // DCR vendors leave default_scope NULL — the AS supplies its own
    // scope set at registration. This lock guards against an
    // over-eager future migration accidentally populating these.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let notion = store
        .get_for_org(db.default_org_id, &cat("notion"))
        .await
        .expect("get_for_org notion")
        .expect("notion row exists");
    assert!(
        notion.default_scope.is_none(),
        "DCR vendors must leave default_scope NULL, got: {:?}",
        notion.default_scope
    );
}
