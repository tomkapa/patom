//! Trait-contract tests for [`relay_rs::mcp::PgMcpServerStore`]. Each test owns its
//! own schema via `TestDb::fresh` so they can run in parallel.
//!
//! Note: every `McpServerCreate` requires a matching row in `mcp_catalog`
//! (the validation trigger added by migration 31). Migration 30 already
//! seeds `notion`, `linear`, `slack`, `jira` globally; tests that need
//! extra ids insert them via [`seed_catalog`] inside the per-test schema.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use relay_rs::clock::SystemClock;
use relay_rs::mcp::{
    ConnectionStatus, DiscoveredTool, McpCatalogId, McpError, McpHealthUpdate, McpHttpUrl,
    McpServerCreate, McpServerId, McpServerStore, McpServerUpdate, McpTransport, PgMcpServerStore,
};

mod common;
use common::pg::TestDb;

fn store(db: &TestDb) -> Arc<PgMcpServerStore> {
    Arc::new(PgMcpServerStore::new(
        db.pool.clone(),
        SystemClock::shared(),
    ))
}

fn http_transport(url: &str) -> McpTransport {
    McpTransport::Http {
        url: McpHttpUrl::try_from(url).expect("valid url"),
    }
}

fn cat(s: &str) -> McpCatalogId {
    McpCatalogId::try_from(s).expect("valid catalog id")
}

/// Insert a global `mcp_catalog` row for the test. Tests that re-use
/// the migration-seeded `notion` / `linear` / `slack` / `jira` ids can
/// skip this; tests that need bespoke ids call it first.
async fn seed_catalog(db: &TestDb, id: &McpCatalogId) {
    sqlx::query(
        "INSERT INTO mcp_catalog \
            (id, org_id, display_name, description, default_transport, auth_kind) \
         VALUES ($1, NULL, $1, $1, '{\"type\":\"http\",\"url\":\"https://example.com/mcp\"}'::jsonb, 'none') \
         ON CONFLICT DO NOTHING",
    )
    .bind(id.as_str())
    .execute(&db.pool)
    .await
    .expect("seed mcp_catalog");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_read_roundtrip() {
    let db = TestDb::fresh().await;
    let store = store(&db);

    let payload = McpServerCreate {
        org_id: db.default_org_id,
        created_by_user_id: db.default_user_id,
        catalog_id: cat("notion"),
        config: http_transport("http://localhost:9000/"),
        description: None,
        enabled: true,
        connection_status: ConnectionStatus::Ok,
    };
    let row = store.create(payload).await.expect("create");
    let read = store.read(row.id, db.default_org_id).await.expect("read");
    assert_eq!(read.id, row.id);
    assert_eq!(read.catalog_id.as_str(), "notion");
    assert!(read.enabled);
    assert_eq!(read.last_seen_at, None);
    assert_eq!(read.last_error, None);
    assert_eq!(read.discovered_tools, None);
    assert_eq!(read.created_by_user_id, db.default_user_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_catalog_id_is_rejected() {
    let db = TestDb::fresh().await;
    let store = store(&db);

    store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("linear"),
            config: http_transport("http://localhost:9000/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("first create");
    let err = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("linear"),
            config: http_transport("http://localhost:9001/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect_err("second create");
    assert!(matches!(err, McpError::CatalogIdTaken(ref a) if a.as_str() == "linear"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_catalog_id_is_rejected() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    // No mcp_catalog row for "phantom" — the validation trigger rejects.
    let err = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("phantom"),
            config: http_transport("http://localhost:9000/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect_err("create with unknown catalog id");
    assert!(matches!(err, McpError::CatalogIdUnknown(ref a) if a.as_str() == "phantom"));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_orders_by_catalog_id() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    // Use the four migration-seeded ids. Disable "slack" so list_enabled
    // can prove the partial-index filter.
    for (name, enabled) in [("notion", true), ("jira", true), ("slack", false)] {
        store
            .create(McpServerCreate {
                org_id: db.default_org_id,
                created_by_user_id: db.default_user_id,
                catalog_id: cat(name),
                config: http_transport(&format!("http://localhost:9000/{name}")),
                description: None,
                enabled,
                connection_status: ConnectionStatus::Ok,
            })
            .await
            .expect("create");
    }
    let all = store.list().await.expect("list");
    let names: Vec<&str> = all.iter().map(|r| r.catalog_id.as_str()).collect();
    assert_eq!(names, vec!["jira", "notion", "slack"]);
    let enabled = store.list_enabled().await.expect("list_enabled");
    let enabled_names: Vec<&str> = enabled.iter().map(|r| r.catalog_id.as_str()).collect();
    assert_eq!(enabled_names, vec!["jira", "notion"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_enabled_skips_auth_pending_rows() {
    // Regression for the "Auth required" warning on freshly-created
    // OAuth servers: rows that are enabled but still mid-OAuth-flow
    // (connection_status = AuthPending) must not be returned to the
    // registry refresher, otherwise it connects without a Bearer token
    // and the upstream's 401 lands as a misleading `last_error`.
    let db = TestDb::fresh().await;
    let store = store(&db);
    store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("notion"),
            config: http_transport("http://localhost:9000/ready"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create ready");
    store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("linear"),
            config: http_transport("http://localhost:9000/pending"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::AuthPending,
        })
        .await
        .expect("create pending");

    let enabled = store.list_enabled().await.expect("list_enabled");
    let names: Vec<&str> = enabled.iter().map(|r| r.catalog_id.as_str()).collect();
    assert_eq!(names, vec!["notion"]);

    let all = store.list().await.expect("list");
    let all_names: Vec<&str> = all.iter().map(|r| r.catalog_id.as_str()).collect();
    assert_eq!(all_names, vec!["linear", "notion"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_changes_config_only() {
    // `catalog_id` is immutable post-create — the UNIQUE constraint and
    // FK trigger together make a rename equivalent to delete + create.
    // The update path only edits the mutable subset (config, description,
    // enabled).
    let db = TestDb::fresh().await;
    let store = store(&db);
    let row = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("slack"),
            config: http_transport("http://localhost:9000/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create");
    let updated = store
        .update(
            row.id,
            db.default_org_id,
            McpServerUpdate {
                config: Some(http_transport("http://localhost:9100/")),
                description: None,
                enabled: Some(false),
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.catalog_id.as_str(), "slack");
    assert!(!updated.enabled);
    let read = store.read(row.id, db.default_org_id).await.expect("read");
    let McpTransport::Http { url, .. } = &read.config;
    assert_eq!(url.as_str(), "http://localhost:9100/");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_returns_not_found_after() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let row = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: cat("jira"),
            config: http_transport("http://localhost:9000/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create");
    store
        .delete(row.id, db.default_org_id)
        .await
        .expect("delete");
    let err = store
        .read(row.id, db.default_org_id)
        .await
        .expect_err("read after delete");
    assert!(matches!(err, McpError::NotFound(_)));
    let err = store
        .delete(row.id, db.default_org_id)
        .await
        .expect_err("delete again");
    assert!(matches!(err, McpError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn update_health_persists_discovered_tools() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    // Bespoke catalog id — exercises the seed_catalog helper.
    let health = cat("health");
    seed_catalog(&db, &health).await;
    let row = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: health,
            config: http_transport("http://localhost:9000/"),
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create");
    let now = chrono::Utc::now();
    let discovered = vec![DiscoveredTool {
        remote_name: "echo".into(),
        prefixed_name: "mcp_health_echo".into(),
        description: Some("echoes input".into()),
    }];
    store
        .update_health(
            row.id,
            db.default_org_id,
            McpHealthUpdate {
                last_seen_at: Some(now),
                last_error: None,
                discovered_tools: Some(discovered.clone()),
            },
        )
        .await
        .expect("update_health");
    let read = store.read(row.id, db.default_org_id).await.expect("read");
    assert!(read.last_seen_at.is_some());
    assert_eq!(read.last_error, None);
    let tools = read.discovered_tools.expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].remote_name, "echo");
    assert_eq!(tools[0].prefixed_name, "mcp_health_echo");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_id_returns_not_found_on_read_update_delete() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let id = McpServerId::new();
    assert!(matches!(
        store.read(id, db.default_org_id).await.expect_err("read"),
        McpError::NotFound(_)
    ));
    assert!(matches!(
        store
            .update(id, db.default_org_id, McpServerUpdate::default())
            .await
            .expect_err("update"),
        McpError::NotFound(_)
    ));
    assert!(matches!(
        store
            .delete(id, db.default_org_id)
            .await
            .expect_err("delete"),
        McpError::NotFound(_)
    ));
}
