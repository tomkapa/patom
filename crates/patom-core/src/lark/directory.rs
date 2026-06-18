//! Persistent people directory for the Lark adapter — `lark_user_handles`.
//!
//! Unlike the Slack adapter (where a Patom account is born from the IdP and a
//! Slack user merely *binds* to it — issue #41), Lark members never log into
//! Patom. The integration is admin-only: members chat in Lark and are
//! addressed by an agent that needs a stable, org-local identity for each of
//! them. So on the first sight of a Lark user this module **mints a shadow** —
//! a synthetic `users` row (with a `…@shadow.invalid` email and no
//! `user_identities`, so it can never authenticate), an `org_members` grant
//! (which fires the `org_members_mint_colleague` trigger, migration 58, to
//! create the Human colleague the agent addresses), and a `lark_user_handles`
//! row keyed on `(tenant_key, lark_user_id)`. The Lark `user_id` (the tenant
//! employee id) is the stable key; the `open_id` is the per-app `@`-tag handle,
//! carried and refreshed opportunistically.
//!
//! Every write touches `users` / `org_members`, which are REVOKEd from the
//! `patom_app` role, so the whole mint runs in one [`run_privileged`]
//! transaction (RLS bypassed) — and it is the *one genuinely new behavior* in
//! the Lark adapter relative to Slack, which provisions nothing.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, UserId, run_privileged};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;

use super::error::LarkError;
use super::limits::LARK_TAG_HANDLES_MAX;
use super::types::{LarkOpenId, LarkUserId, TenantKey};

/// A shadow colleague resolved (or just minted) for a Lark user.
///
/// Pairs the minted Human [`ColleagueId`] with the synthetic [`UserId`] that
/// backs it, so the caller can address the colleague and, when needed, the
/// underlying shadow user without a second lookup.
#[derive(Debug, Clone, Copy)]
pub struct ShadowColleague {
    pub colleague_id: ColleagueId,
    pub user_id: UserId,
}

#[async_trait]
pub trait LarkDirectory: fmt::Debug + Send + Sync {
    /// Resolve the colleague for a Lark user in a tenant, minting a shadow on
    /// first sight.
    ///
    /// On a miss this creates a synthetic `users` row, an `org_members` grant
    /// (which mints the Human colleague via the migration-58 trigger), and the
    /// `lark_user_handles` row — all in one privileged transaction. Idempotent
    /// on `(tenant_key, lark_user_id)`: a repeat call returns the existing
    /// colleague and opportunistically refreshes a changed `open_id`. `name`
    /// (e.g. from the roster) seeds the synthetic user's `display_name` when
    /// present.
    async fn resolve_or_mint(
        &self,
        org_id: OrgId,
        tenant_key: &TenantKey,
        user_id: &LarkUserId,
        open_id: &LarkOpenId,
        name: Option<&str>,
    ) -> Result<ShadowColleague, LarkError>;

    /// Reverse lookup: the `@`-tag handle (`open_id`) for a colleague in an org.
    ///
    /// `Ok(None)` when the colleague is not a Lark shadow (no
    /// `lark_user_handles` row in this org); `Err` only on DB / parse failures.
    async fn open_id_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<LarkOpenId>, LarkError>;

    /// All `(display_name, open_id)` pairs for the org's Lark humans, for
    /// rewriting `@Name` into `<at>` markup on an outbound reply.
    ///
    /// The name is the canonical colleague display name (matching what the
    /// agent sees in its roster). Bounded by [`LARK_TAG_HANDLES_MAX`].
    async fn taggable_handles(&self, org_id: OrgId)
    -> Result<Vec<(String, LarkOpenId)>, LarkError>;

    /// The `(display_name, open_id)` for a single colleague, or `None` if it is
    /// not a Lark shadow — for `<at>`-tagging the addressed recipient of a
    /// `send_message` (whose name may not appear in the reply text).
    async fn tag_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<(String, LarkOpenId)>, LarkError>;

    /// The `agents.name` for an *agent* colleague in an org, or `None` for a
    /// human / unknown colleague.
    ///
    /// A peer bot cannot be `<at>`-pinged in the BYO multi-app model — its
    /// `open_id` is app-scoped and undiscoverable across apps (Feishu docs) — so
    /// when a `send_message` addresses an agent, the outbound render falls back
    /// to a plain `@Name` text marker (a visible "to whom" cue, not a real
    /// ping). This resolves that name. Privileged + org-scoped.
    async fn agent_name_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<String>, LarkError>;

    /// Reverse lookup for the card-action callback (#214): the colleague behind a
    /// clicking Lark user's `open_id` in an org, or `None` when that `open_id` has
    /// no handle here. Privileged + org-scoped (the callback carries no `Caller`).
    /// The `open_id` already minted a colleague when the user first appeared, so
    /// this is a plain read — no mint on the click path.
    async fn colleague_for_open_id(
        &self,
        org_id: OrgId,
        open_id: &LarkOpenId,
    ) -> Result<Option<ColleagueId>, LarkError>;
}

/// Shared handle to a [`LarkDirectory`].
pub type SharedLarkDirectory = Arc<dyn LarkDirectory>;

/// Postgres-backed [`LarkDirectory`].
///
/// Holds a [`PgPool`] and a [`SharedClock`]; all writes are timestamped from
/// the clock (§11) and run privileged because the shadow mint touches `users`
/// and `org_members` (REVOKEd from `patom_app`).
pub struct PgLarkDirectory {
    pool: PgPool,
    clock: SharedClock,
}

impl PgLarkDirectory {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgLarkDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLarkDirectory").finish_non_exhaustive()
    }
}

#[async_trait]
impl LarkDirectory for PgLarkDirectory {
    async fn resolve_or_mint(
        &self,
        org_id: OrgId,
        tenant_key: &TenantKey,
        user_id: &LarkUserId,
        open_id: &LarkOpenId,
        name: Option<&str>,
    ) -> Result<ShadowColleague, LarkError> {
        let now = self.clock.now_utc();
        let tenant = tenant_key.as_str().to_owned();
        let lark_user = user_id.as_str().to_owned();
        let open = open_id.as_str().to_owned();
        let display_name = name.map(str::to_owned);
        // Synthetic, unroutable email: there is no `user_identities` row, so
        // this account can never authenticate. The `lark-` prefix + the stable
        // tenant `user_id` make it deterministic and collision-free per user.
        let synthetic_email = format!("lark-{lark_user}@shadow.invalid");

        // One privileged transaction: `users` / `org_members` are REVOKEd from
        // the app role, and the membership insert fires the colleague-mint
        // trigger — so all writes must land together, RLS-free, atomically.
        run_privileged::<ShadowColleague, LarkError>(&self.pool, async move |tx| {
            // 1. Fast path: an existing handle → its colleague + shadow user.
            //    Opportunistically refresh a changed `open_id` on the way out.
            let existing: Option<(ColleagueId, UserId)> = sqlx::query_as(
                "SELECT h.colleague_id, c.user_id \
                   FROM lark_user_handles h \
                   JOIN colleagues c ON c.id = h.colleague_id \
                  WHERE h.tenant_key = $1 AND h.lark_user_id = $2",
            )
            .bind(&tenant)
            .bind(&lark_user)
            .fetch_optional(&mut **tx)
            .await?;

            if let Some((colleague_id, shadow_user)) = existing {
                sqlx::query(
                    "UPDATE lark_user_handles SET open_id = $3 \
                      WHERE tenant_key = $1 AND lark_user_id = $2 AND open_id <> $3",
                )
                .bind(&tenant)
                .bind(&lark_user)
                .bind(&open)
                .execute(&mut **tx)
                .await?;
                // Backfill the display name when we learn it (e.g. a later
                // roster sync, after the shadow was first minted from a
                // name-less message event) and it's still unset — so the agent's
                // roster shows the real name, not the `lark-…` email local-part.
                if let Some(name) = display_name.as_deref() {
                    sqlx::query(
                        "UPDATE users SET display_name = $2, updated_at = $3 \
                          WHERE id = $1 AND display_name IS NULL",
                    )
                    .bind(shadow_user)
                    .bind(name)
                    .bind(now)
                    .execute(&mut **tx)
                    .await?;
                }
                return Ok(ShadowColleague {
                    colleague_id,
                    user_id: shadow_user,
                });
            }

            // 2. Mint the synthetic user. No `user_identities` row is written,
            //    so the account is display-only and can never authenticate.
            let (new_user,): (UserId,) = sqlx::query_as(
                "INSERT INTO users (id, email, display_name, created_at, updated_at) \
                 VALUES (gen_random_uuid(), $1, $2, $3, $3) \
                 RETURNING id",
            )
            .bind(&synthetic_email)
            .bind(display_name.as_deref())
            .bind(now)
            .fetch_one(&mut **tx)
            .await?;

            // 3. Grant org membership → fires `org_members_mint_colleague`
            //    (migration 58), creating the Human colleague in this same tx.
            sqlx::query(
                "INSERT INTO org_members (org_id, user_id, role, created_at) \
                 VALUES ($1, $2, 'member', $3)",
            )
            .bind(org_id)
            .bind(new_user)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            // 4. Read back the colleague the trigger just minted.
            let (colleague_id,): (ColleagueId,) =
                sqlx::query_as("SELECT id FROM colleagues WHERE org_id = $1 AND user_id = $2")
                    .bind(org_id)
                    .bind(new_user)
                    .fetch_one(&mut **tx)
                    .await?;

            // 5. Record the handle. ON CONFLICT keeps the mint idempotent under
            //    a concurrent first-sight race (refresh the `open_id`).
            sqlx::query(
                "INSERT INTO lark_user_handles \
                   (org_id, tenant_key, lark_user_id, open_id, colleague_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (tenant_key, lark_user_id) DO UPDATE SET \
                   open_id = EXCLUDED.open_id",
            )
            .bind(org_id)
            .bind(&tenant)
            .bind(&lark_user)
            .bind(&open)
            .bind(colleague_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            Ok(ShadowColleague {
                colleague_id,
                user_id: new_user,
            })
        })
        .await
    }

    async fn open_id_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<LarkOpenId>, LarkError> {
        // Privileged: the lookup carries no Principal (outbound rendering path)
        // and is already org-scoped by the bound `org_id`.
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT open_id FROM lark_user_handles \
                      WHERE org_id = $1 AND colleague_id = $2",
                )
                .bind(org_id)
                .bind(colleague)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;

        match row {
            Some((open_id,)) => Ok(Some(LarkOpenId::try_from(open_id)?)),
            None => Ok(None),
        }
    }

    async fn colleague_for_open_id(
        &self,
        org_id: OrgId,
        open_id: &LarkOpenId,
    ) -> Result<Option<ColleagueId>, LarkError> {
        // Privileged (no Principal on the card-callback path) + org-scoped.
        let open_id = open_id.as_str().to_owned();
        let row: Option<(ColleagueId,)> =
            run_privileged::<Option<(ColleagueId,)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT colleague_id FROM lark_user_handles \
                      WHERE org_id = $1 AND open_id = $2",
                )
                .bind(org_id)
                .bind(open_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.map(|(c,)| c))
    }

    async fn taggable_handles(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(String, LarkOpenId)>, LarkError> {
        // The name must match what the agent sees in its roster: the canonical
        // colleague display name (users.display_name, else the email
        // local-part). Privileged (no Principal; `users` is REVOKEd from the
        // app role) + org-scoped + bounded (§5).
        let cap = i64::try_from(LARK_TAG_HANDLES_MAX).unwrap_or(i64::MAX);
        let rows: Vec<(String, String)> =
            run_privileged::<Vec<(String, String)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT COALESCE(u.display_name, split_part(u.email, '@', 1)) AS name, \
                            h.open_id \
                       FROM lark_user_handles h \
                       JOIN colleagues c ON c.id = h.colleague_id \
                       JOIN users u       ON u.id = c.user_id \
                      WHERE h.org_id = $1 \
                      LIMIT $2",
                )
                .bind(org_id)
                .bind(cap)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (name, open_id) in rows {
            out.push((name, LarkOpenId::try_from(open_id)?));
        }
        Ok(out)
    }

    async fn tag_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<(String, LarkOpenId)>, LarkError> {
        let row: Option<(String, String)> =
            run_privileged::<Option<(String, String)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT COALESCE(u.display_name, split_part(u.email, '@', 1)) AS name, \
                            h.open_id \
                       FROM lark_user_handles h \
                       JOIN colleagues c ON c.id = h.colleague_id \
                       JOIN users u       ON u.id = c.user_id \
                      WHERE h.org_id = $1 AND h.colleague_id = $2",
                )
                .bind(org_id)
                .bind(colleague)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        match row {
            Some((name, open_id)) => Ok(Some((name, LarkOpenId::try_from(open_id)?))),
            None => Ok(None),
        }
    }

    async fn agent_name_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<String>, LarkError> {
        // Privileged (no Principal on the outbound render path) + org-scoped by
        // the bound `$1`. The `kind = 'agent'` filter (with the join on
        // `agent_id`) returns `None` for a human / unknown colleague.
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT ag.name \
                       FROM colleagues c \
                       JOIN agents ag ON ag.id = c.agent_id \
                      WHERE c.id = $2 AND c.org_id = $1 AND c.kind = 'agent'",
                )
                .bind(org_id)
                .bind(colleague)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.map(|(name,)| name))
    }
}
