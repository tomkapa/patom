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
    // One connection for the whole run: the `search_path` we set below is
    // session state, so it must persist across the Migrator's per-migration
    // transactions — that only holds if every statement runs on the same
    // connection.
    let mut conn = pool
        .acquire()
        .await
        .map_err(|source| AppError::DbConnect { source })?;

    sqlx::query("CREATE SCHEMA IF NOT EXISTS cloud")
        .execute(&mut *conn)
        .await
        .map_err(|e| AppError::Misconfigured(format!("create cloud schema: {e}")))?;
    sqlx::query("SET search_path TO cloud, public")
        .execute(&mut *conn)
        .await
        .map_err(|e| AppError::Misconfigured(format!("pin cloud search_path: {e}")))?;

    let result = MIGRATOR.run(&mut *conn).await;

    // Return the connection to the pool clean: leaving `search_path` pinned to
    // `cloud, public` would subtly change unqualified name resolution for
    // whatever core query grabs this connection next. Best-effort — if the run
    // already failed, boot aborts regardless.
    let _ = sqlx::query("SET search_path TO DEFAULT")
        .execute(&mut *conn)
        .await;

    result.map_err(|source| AppError::Migrate { source })
}
