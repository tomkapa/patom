//! Round-trip test for `mcp_catalog.client_source` + `platform_client_alias`.
//!
//! Migration 50 seeds `gmail` with `client_source='platform'` and
//! `platform_client_alias='google'`. This test pins that the new columns
//! survive a full DB round-trip through the catalog store and decode into
//! the `McpCatalogEntry` shape the resolver consumes.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::mcp::{
    ClientSource, McpCatalogId, McpCatalogStore, PgMcpCatalogStore, platform_env_keys,
};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> Arc<PgMcpCatalogStore> {
    Arc::new(PgMcpCatalogStore::new(pool.clone()))
}

fn cat(s: &str) -> McpCatalogId {
    McpCatalogId::try_from(s).expect("valid catalog id")
}

#[sqlx::test]
async fn round_trips_client_source_and_alias(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // `gmail` is seeded by migration 50 with platform + alias='google'.
    let entry = store
        .get_for_org(seed.org_id, &cat("gmail"))
        .await
        .expect("query")
        .expect("gmail seeded");
    assert_eq!(entry.client_source, ClientSource::Platform);
    let alias = entry.platform_client_alias.expect("alias set on gmail");
    assert_eq!(alias.as_str(), "google");

    // `github` is platform but uses its own client (no alias).
    let entry = store
        .get_for_org(seed.org_id, &cat("github"))
        .await
        .expect("query")
        .expect("github seeded");
    assert_eq!(entry.client_source, ClientSource::Platform);
    assert!(entry.platform_client_alias.is_none());

    // `notion` defaults to `dcr` (the column default).
    let entry = store
        .get_for_org(seed.org_id, &cat("notion"))
        .await
        .expect("query")
        .expect("notion seeded");
    assert_eq!(entry.client_source, ClientSource::Dcr);
    assert!(entry.platform_client_alias.is_none());
}

#[test]
fn platform_env_keys_uppercases_and_replaces_dashes() {
    // Catalog ids use lowercase + `-` / `_`; env-var middles use upper
    // with `-` → `_`. The two-letter convention is total (CatalogId regex
    // already excludes everything that would break it).
    let id = cat("google");
    assert_eq!(
        platform_env_keys(&id),
        (
            "PATOM_GOOGLE_CLIENT_ID".to_string(),
            "PATOM_GOOGLE_CLIENT_SECRET".to_string(),
        )
    );

    let id = cat("microsoft-365");
    assert_eq!(
        platform_env_keys(&id),
        (
            "PATOM_MICROSOFT_365_CLIENT_ID".to_string(),
            "PATOM_MICROSOFT_365_CLIENT_SECRET".to_string(),
        )
    );

    let id = cat("github");
    assert_eq!(
        platform_env_keys(&id),
        (
            "PATOM_GITHUB_CLIENT_ID".to_string(),
            "PATOM_GITHUB_CLIENT_SECRET".to_string(),
        )
    );
}
