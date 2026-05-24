//! Authentication + tenancy primitives.
//!
//! - Identity types ([`UserId`], [`OrgId`], [`Email`], [`Role`], [`Principal`]).
//! - JWT-in-cookie session ([`JwtSigner`]).
//! - Google OAuth wrapper ([`GoogleOAuth`]).
//! - Store contract over `users`, `organizations`, `org_members`,
//!   `user_identities`, `oauth_login_states` ([`UserStore`]).
//! - Tenant-context helpers ([`TenantTx`], [`PrivilegedTx`], [`run_as_user`],
//!   [`run_privileged`]).

mod error;
mod jwt;
mod language;
pub mod limits;
mod locale_hint;
mod oauth_google;
mod org_language;
mod org_rule;
mod pg_store;
mod store;
mod types;

pub use error::AuthError;
pub use jwt::{JwtClaims, JwtSigner};
pub use language::Language;
pub use locale_hint::LocaleHint;
pub use oauth_google::{AuthStart, GoogleOAuth, TokenExchanger};
pub use org_language::{
    LanguageResolverError, OrgLanguageResolver, PgOrgLanguageResolver, SharedOrgLanguageResolver,
};
pub use org_rule::{
    OrgRuleResolver, OrganizationRule, PgOrgRuleResolver, RuleResolverError, SharedOrgRuleResolver,
};
pub use pg_store::PgUserStore;
pub use store::{
    ConsumedOAuthState, NewOrg, OAuthStateRow, SharedUserStore, UpsertedUser, UserStore,
};
pub use types::{
    Email, GoogleProfile, GoogleSubject, OAuthState, OrgId, OrgMembership, OrgSlug, PkceVerifier,
    Principal, Role, User, UserId,
};

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// Tenant-scoped transaction.
///
/// The wrapper is the proof that `SET LOCAL app.user_id` ran and that we
/// dropped to the non-super `relay_app` role, so RLS policies on every
/// domain table fire as expected. The only way to mint one is
/// [`run_as_user`].
#[derive(Debug)]
pub struct TenantTx<'a> {
    inner: Transaction<'a, Postgres>,
    user: UserId,
}

impl<'a> TenantTx<'a> {
    fn new(inner: Transaction<'a, Postgres>, user: UserId) -> Self {
        Self { inner, user }
    }

    /// The user id pinned into `app.user_id` for this transaction.
    /// Stores reach for it when stamping audit columns
    /// (`created_by_user_id`, `acting_user_id`) so the value can't
    /// disagree with the GUC RLS reads.
    #[must_use]
    pub fn acting_user(&self) -> UserId {
        self.user
    }

    /// Hand the inner `Transaction` to a private `_in_tx` helper. The
    /// returned `&mut Transaction` proof keeps "you are inside a tx"
    /// a compile-time invariant at every helper-call boundary —
    /// `&mut PgConnection` from the pool can't masquerade as one.
    pub(crate) fn tx_mut(&mut self) -> &mut Transaction<'a, Postgres> {
        &mut self.inner
    }

    async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

/// `Deref` targets `PgConnection` (not `Transaction`) so the
/// `&mut **tx` idiom used everywhere with `&mut Transaction` parameters
/// keeps working unchanged when the parameter type becomes
/// `&mut TenantTx<'_>`. The runner owns commit/rollback, so users have
/// no business reaching for the underlying `Transaction`.
impl std::ops::Deref for TenantTx<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for TenantTx<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Privileged transaction. RLS is bypassed via `SET LOCAL row_security = off`.
///
/// Use only for infrastructure paths (queue claim, scheduler scans, worker
/// fan-out) where the tenant context cannot be derived from a single
/// principal. The only way to mint one is [`run_privileged`].
#[derive(Debug)]
pub struct PrivilegedTx<'a> {
    inner: Transaction<'a, Postgres>,
}

impl<'a> PrivilegedTx<'a> {
    fn new(inner: Transaction<'a, Postgres>) -> Self {
        Self { inner }
    }

    /// Hand the inner `Transaction` to a private `_in_tx` helper. See
    /// [`TenantTx::tx_mut`] for the rationale.
    pub(crate) fn tx_mut(&mut self) -> &mut Transaction<'a, Postgres> {
        &mut self.inner
    }

    async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

impl std::ops::Deref for PrivilegedTx<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for PrivilegedTx<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Open a transaction with `app.user_id` set to the principal's user id.
///
/// `SET LOCAL` (the `set_config(..., true)` SQL form) scopes the value
/// to this transaction so a connection returned to the pool without
/// commit/rollback cannot leak the value to the next checkout. RLS
/// policies on every domain table read `app_current_user_id()` and
/// check membership through `app_user_is_member(row.org_id)`, so
/// subsequent queries automatically filter to rows the principal can
/// see.
#[allow(clippy::elidable_lifetime_names)]
pub async fn begin_as_user<'a>(
    pool: &'a PgPool,
    user_id: UserId,
) -> Result<Transaction<'a, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.user_id', $1, true)")
        .bind(user_id.as_uuid().to_string())
        .execute(&mut *tx)
        .await?;
    // Drop privileges to the non-super `relay_app` role so RLS policies
    // actually apply. Postgres superusers bypass RLS unconditionally —
    // even with FORCE ROW LEVEL SECURITY — so the table owner identity
    // most apps use in dev (here, `relay`) wouldn't be isolated without
    // this. RESET ROLE happens automatically on commit/rollback.
    sqlx::query("SET LOCAL ROLE relay_app")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Open a transaction that bypasses RLS. Use only for infrastructure
/// code (queue claim, scheduler scans, migrations) — never for HTTP
/// request handling.
///
/// Works because the app role is the table owner in dev and tests;
/// production deployments that split roles should swap this for a
/// dedicated `BYPASSRLS` role.
pub async fn begin_privileged(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL row_security = off")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Run `f` inside a tenant-scoped transaction.
///
/// The closure receives a `&mut TenantTx<'_>`; the runner opens the tx
/// (with `SET LOCAL app.user_id` + `SET LOCAL ROLE relay_app`), invokes
/// `f`, and commits on `Ok` or rolls back on `Err`. `.commit()` does not
/// exist at call sites.
///
/// The `E: From<sqlx::Error>` bound lets every subsystem error enum's
/// existing `Db(#[from] sqlx::Error)` variant absorb the commit /
/// rollback failure via `?`.
pub async fn run_as_user<T, E>(
    pool: &PgPool,
    user_id: UserId,
    f: impl AsyncFnOnce(&mut TenantTx<'_>) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<sqlx::Error>,
{
    let raw = begin_as_user(pool, user_id).await.map_err(E::from)?;
    let mut tx = TenantTx::new(raw, user_id);
    match f(&mut tx).await {
        Ok(value) => {
            tx.commit().await.map_err(E::from)?;
            Ok(value)
        }
        Err(err) => {
            // Best-effort rollback; sqlx also rolls back on drop, but
            // an explicit call gives the failure a span and surfaces a
            // secondary error (rare; we still propagate the original).
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

/// Run `f` inside a privileged (RLS-bypassing) transaction. Same
/// commit/rollback contract as [`run_as_user`].
pub async fn run_privileged<T, E>(
    pool: &PgPool,
    f: impl AsyncFnOnce(&mut PrivilegedTx<'_>) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<sqlx::Error>,
{
    let raw = begin_privileged(pool).await.map_err(E::from)?;
    let mut tx = PrivilegedTx::new(raw);
    match f(&mut tx).await {
        Ok(value) => {
            tx.commit().await.map_err(E::from)?;
            Ok(value)
        }
        Err(err) => {
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

/// Open a tenant-scoped transaction for the given principal.
///
/// Kept as a building block for the rare caller that needs an
/// externally-managed transaction lifecycle — prefer [`run_as_user`]
/// which owns commit/rollback. See [`begin_as_user`] for the
/// worker-side variant that takes a `UserId` directly.
#[allow(clippy::elidable_lifetime_names)]
pub async fn begin_as<'a>(
    pool: &'a PgPool,
    principal: &Principal,
) -> Result<Transaction<'a, Postgres>, AuthError> {
    begin_as_user(pool, principal.user_id)
        .await
        .map_err(AuthError::from)
}

/// `(user_id, org_id)` pair carried by tool / worker code paths.
///
/// Replaces the historical pattern of passing
/// `(acting_user_id, org_id, created_by_user_id)` as three separate
/// arguments — those values are definitionally equal (the worker
/// derives `acting_user_id` from `sessions.created_by_user_id`, child
/// sessions inherit the parent's `(org_id, created_by_user_id)`), so
/// one struct is both more honest and lighter at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub user_id: UserId,
    pub org_id: OrgId,
}

impl Caller {
    #[must_use]
    pub fn new(user_id: UserId, org_id: OrgId) -> Self {
        Self { user_id, org_id }
    }
}

/// Domain tables that a route may probe for tenant visibility. The
/// variant maps to a `&'static str` so the table name in the probe SQL
/// is never user-controlled (CLAUDE.md §10).
#[derive(Debug, Clone, Copy)]
pub enum VisibilityTable {
    Agents,
    McpServers,
    PromptRequests,
}

impl VisibilityTable {
    const fn table(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::McpServers => "mcp_servers",
            Self::PromptRequests => "prompt_requests",
        }
    }
}

/// Tenant-visibility probe shared by the HTTP routes.
///
/// Routes that delegate to a privileged-tx store must first 404
/// cross-org / unknown ids without leaking existence. Opens a tenant
/// tx so RLS filters to rows the principal can see, runs
/// `SELECT EXISTS(SELECT 1 FROM <table> WHERE id = $1)`, commits, and
/// returns the boolean.
pub async fn visible_to(
    pool: &PgPool,
    principal: &Principal,
    table: VisibilityTable,
    id: uuid::Uuid,
) -> Result<bool, AuthError> {
    run_as_user(pool, principal.user_id, async |tx| {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE id = $1)",
            table.table()
        );
        let exists: bool = sqlx::query_scalar(&sql)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        Ok::<bool, AuthError>(exists)
    })
    .await
}
