//! The `cloud` schema's own migration stream.
//!
//! Billing tables live in a dedicated `cloud` schema with their **own**
//! `cloud._sqlx_migrations` tracking table — never the core `public` stream —
//! so a future "move billing to its own database" is a clean schema move
//! (issue #131). sqlx 0.8's `Migrator` has no API to name the tracking table's
//! schema, so we pin the connection's `search_path` to `cloud, public` for the
//! duration of the run: sqlx then creates `_sqlx_migrations` in the first
//! schema on the path (`cloud`), and the policies can still reference the
//! `public` membership helper. `public` is kept on the path so
//! `public.app_user_is_member` resolves during `CREATE POLICY`.

use patom::AppError;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::time::timeout;

use crate::lemon_squeezy::limits::MIGRATION_TIMEOUT;

/// Billing migrations embedded at compile time. Versioned in a high, distinct
/// range (`2000000000000x`) so that — even if this stream were ever pointed at
/// the wrong schema by mistake — its versions can never collide-mismatch with
/// the core `public._sqlx_migrations` checksums.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Create the `cloud` schema and run the billing migrations into it.
///
/// Idempotent; tracked by `cloud._sqlx_migrations`. Call once at startup,
/// **after** the core migration stream — the billing RLS policies reference
/// `public.app_user_is_member`, which core migration 14 defines.
///
/// # Errors
/// Returns [`AppError::Migrate`] if a migration fails, or
/// [`AppError::Misconfigured`] if the schema / `search_path` setup fails.
pub async fn run_migrations(pool: &PgPool) -> Result<(), AppError> {
    // Every startup I/O await below is bounded by MIGRATION_TIMEOUT (§5) so a
    // stalled DB can't hang boot indefinitely; an elapsed timeout surfaces as a
    // descriptive Misconfigured error.
    let bound =
        |what: &'static str| move |_elapsed| AppError::Misconfigured(format!("{what} timed out"));

    // The schema must exist before the Migrator creates its tracking table
    // (which lands in the first schema on `search_path`).
    timeout(
        MIGRATION_TIMEOUT,
        sqlx::query("CREATE SCHEMA IF NOT EXISTS cloud").execute(pool),
    )
    .await
    .map_err(bound("create cloud schema"))?
    .map_err(|e| AppError::Misconfigured(format!("create cloud schema: {e}")))?;

    // A short-lived pool whose every connection pins `search_path` to
    // `cloud, public`: sqlx then writes `cloud._sqlx_migrations` (first schema
    // on the path) and the policies still resolve `public.app_user_is_member`.
    // Single connection — this runs once at boot. Closed at the end so no pinned
    // connection lingers. Running the Migrator on a `&Pool` (not a
    // `&mut Connection`) keeps this `Send` — the connection form trips a sqlx
    // `Acquire is not general enough` bound under the `#[async_trait]` caller.
    let cloud_pool = timeout(
        MIGRATION_TIMEOUT,
        PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO cloud, public")
                        .execute(conn)
                        .await
                        .map(|_| ())
                })
            })
            .connect_with((*pool.connect_options()).clone()),
    )
    .await
    .map_err(bound("connect cloud migration pool"))?
    .map_err(|source| AppError::DbConnect { source })?;

    let result = timeout(MIGRATION_TIMEOUT, MIGRATOR.run(&cloud_pool))
        .await
        .map_err(bound("run cloud migrations"))?;
    cloud_pool.close().await;
    result.map_err(|source| AppError::Migrate { source })
}
