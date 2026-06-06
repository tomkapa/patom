//! The `cloud` migration stream is isolated from core's `public` stream.
//!
//! `#[sqlx::test(migrations = "...")]` runs the core migrations into a fresh
//! database (giving us `public.app_user_is_member` + the identity tables the
//! billing RLS references), then the test runs the cloud migrations on top and
//! asserts they land in `cloud` with their own tracking table — never `public`.

#![allow(clippy::expect_used)]

use sqlx::PgPool;

/// Does a table exist in the given schema?
async fn table_exists(pool: &PgPool, schema: &str, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = $2)",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(pool)
    .await
    .expect("information_schema query")
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn cloud_migrations_land_in_their_own_schema(pool: PgPool) {
    patom_cloud::run_migrations(&pool)
        .await
        .expect("cloud migrations run");

    // The billing tracking table lives in `cloud`, not `public`.
    assert!(
        table_exists(&pool, "cloud", "_sqlx_migrations").await,
        "cloud schema must own its _sqlx_migrations tracking table",
    );
    // The billing tables landed in `cloud`.
    assert!(table_exists(&pool, "cloud", "subscriptions").await);
    assert!(table_exists(&pool, "cloud", "webhook_events").await);

    // ...and did NOT leak into `public` (which keeps only the core stream).
    assert!(
        !table_exists(&pool, "public", "subscriptions").await,
        "billing tables must not leak into the public schema",
    );
    assert!(
        table_exists(&pool, "public", "_sqlx_migrations").await,
        "core's public tracking table is untouched",
    );
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn cloud_migrations_are_idempotent(pool: PgPool) {
    // Running twice is a no-op the second time — the version is already
    // recorded in cloud._sqlx_migrations.
    patom_cloud::run_migrations(&pool).await.expect("first run");
    patom_cloud::run_migrations(&pool)
        .await
        .expect("second run is a no-op");
}
