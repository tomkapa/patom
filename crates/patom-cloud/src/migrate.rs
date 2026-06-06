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

use std::future::Future;
use std::pin::Pin;

use patom::AppError;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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
// Returns an explicitly-boxed `Send` future rather than `async fn` on purpose:
// boxing erases the future's type so it can be awaited inside the
// `#[async_trait]` `CloudBuilder::migrate` impl. Crucially the Migrator runs on
// a `&Pool`, not a `&mut Connection` — the connection form trips a sqlx
// `Acquire is not general enough` HRTB bound once a `Send` future is required.
#[allow(clippy::manual_async_fn)]
pub fn run_migrations(
    pool: &PgPool,
) -> Pin<Box<dyn Future<Output = Result<(), AppError>> + Send + '_>> {
    // Reuse the live pool's connection options (host / credentials / db) for the
    // dedicated migration pool below.
    let connect_options = (*pool.connect_options()).clone();
    Box::pin(async move {
        // The schema must exist before the Migrator creates its tracking table
        // (which lands in the first schema on `search_path`).
        sqlx::query("CREATE SCHEMA IF NOT EXISTS cloud")
            .execute(pool)
            .await
            .map_err(|e| AppError::Misconfigured(format!("create cloud schema: {e}")))?;

        // A short-lived pool whose every connection pins `search_path` to
        // `cloud, public`: sqlx then writes `cloud._sqlx_migrations` (first
        // schema on the path) and the policies still resolve
        // `public.app_user_is_member`. Single connection — this runs once at
        // boot. Closed at the end so no pinned connection lingers.
        let cloud_pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO cloud, public")
                        .execute(conn)
                        .await
                        .map(|_| ())
                })
            })
            .connect_with(connect_options)
            .await
            .map_err(|source| AppError::DbConnect { source })?;

        let result = MIGRATOR.run(&cloud_pool).await;
        cloud_pool.close().await;
        result.map_err(|source| AppError::Migrate { source })
    })
}
