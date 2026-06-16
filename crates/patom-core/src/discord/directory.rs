//! Persistent people directory for the Discord adapter — `discord_user_handles`.
//!
//! Like Lark (and unlike Slack), Discord members never log into Patom: the
//! integration is admin-only, and the agent addresses each member by a stable
//! org-local identity. So on the first sight of a Discord user this module
//! **mints a shadow** — a synthetic `users` row (`…@shadow.invalid`, no
//! `user_identities`, can never authenticate), an `org_members` grant (which
//! fires the `org_members_mint_colleague` trigger, migration 58, to create the
//! Human colleague the agent addresses), and a `discord_user_handles` row keyed
//! on `(org_id, discord_user_id)`.
//!
//! Discord is the directory's *easy* case: the user snowflake is **global** (not
//! tenant-scoped like Lark's `user_id`/`open_id` split), so one key serves every
//! source — live events, roster, and history — with no satellite handle. The
//! snowflake is also the `@`-mention id (`<@snowflake>`), so the outbound reverse
//! lookups return it directly.
//!
//! Every write touches `users` / `org_members` (REVOKEd from `patom_app`), so the
//! whole mint runs in one [`run_privileged`] transaction (RLS bypassed).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, UserId, run_privileged};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;

use super::error::DiscordError;
use super::limits::{DISCORD_DISPLAY_NAME_MAX, DISCORD_TAG_HANDLES_MAX};
use super::types::DiscordUserId;

/// A shadow colleague resolved (or just minted) for a Discord user.
#[derive(Debug, Clone, Copy)]
pub struct ShadowColleague {
    pub colleague_id: ColleagueId,
    pub user_id: UserId,
}

#[async_trait]
pub trait DiscordDirectory: fmt::Debug + Send + Sync {
    /// Resolve the colleague for a Discord user in an org, minting a shadow on
    /// first sight.
    ///
    /// On a miss this creates a synthetic `users` row, an `org_members` grant
    /// (which mints the Human colleague via the migration-58 trigger), and the
    /// `discord_user_handles` row — all in one privileged transaction.
    /// Idempotent on `(org_id, discord_user_id)`: a repeat call returns the
    /// existing colleague and refreshes the observed `name` (nick changes), and
    /// backfills the synthetic user's `display_name` when first learned.
    async fn resolve_or_mint(
        &self,
        org_id: OrgId,
        user_id: &DiscordUserId,
        name: Option<&str>,
    ) -> Result<ShadowColleague, DiscordError>;

    /// Reverse lookup: the user snowflake for a colleague in an org, or `None`
    /// when the colleague is not a Discord shadow. The poster renders it as
    /// `<@snowflake>`.
    async fn snowflake_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<DiscordUserId>, DiscordError>;

    /// All `(display_name, snowflake)` pairs for the org's Discord humans, for
    /// rewriting `@Name` into `<@snowflake>` on an outbound reply. Bounded by
    /// [`DISCORD_TAG_HANDLES_MAX`].
    async fn taggable_handles(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(String, DiscordUserId)>, DiscordError>;

    /// The `(display_name, snowflake)` for a single colleague, or `None` if it is
    /// not a Discord shadow — for `<@>`-tagging the addressed recipient of a
    /// `send_message`.
    async fn tag_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<(String, DiscordUserId)>, DiscordError>;

    /// The `agents.name` for an *agent* colleague in an org, or `None` for a
    /// human / unknown colleague — the `@Name` fallback when a `send_message`
    /// addresses a peer agent whose bot has not yet reported its user id.
    async fn agent_name_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<String>, DiscordError>;
}

pub type SharedDiscordDirectory = Arc<dyn DiscordDirectory>;

/// Postgres-backed [`DiscordDirectory`].
pub struct PgDiscordDirectory {
    pool: PgPool,
    clock: SharedClock,
}

impl PgDiscordDirectory {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgDiscordDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDiscordDirectory").finish_non_exhaustive()
    }
}

#[async_trait]
impl DiscordDirectory for PgDiscordDirectory {
    async fn resolve_or_mint(
        &self,
        org_id: OrgId,
        user_id: &DiscordUserId,
        name: Option<&str>,
    ) -> Result<ShadowColleague, DiscordError> {
        let now = self.clock.now_utc();
        let discord_user = user_id.as_str().to_owned();
        // Cap at the trust boundary (§5): roster callers truncate, but a message
        // caller could pass an over-long name straight through to the row.
        let display_name =
            name.map(|n| n.chars().take(DISCORD_DISPLAY_NAME_MAX).collect::<String>());
        // Synthetic, unroutable email: no `user_identities` row, so this account
        // can never authenticate. The global snowflake makes it deterministic.
        let synthetic_email = format!("discord-{discord_user}@shadow.invalid");

        run_privileged::<ShadowColleague, DiscordError>(&self.pool, async move |tx| {
            // 0. Serialize first-sight per (org, user) for the duration of this
            //    transaction. Without it, two concurrent first-sight events for the
            //    same Discord user (e.g. the same message delivered to two of the
            //    org's bot connections) both fall through the fast path and mint a
            //    user/org_member/colleague; the handle upsert's loser keeps the
            //    winner's row but we'd return the loser's orphaned ids. The xact
            //    lock releases on commit/rollback; the waiter then takes the fast
            //    path below and returns the canonical ids.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("discord-shadow:{org_id}:{discord_user}"))
                .execute(&mut **tx)
                .await?;

            // 1. Fast path: an existing handle → its colleague + shadow user.
            let existing: Option<(ColleagueId, UserId)> = sqlx::query_as(
                "SELECT h.colleague_id, c.user_id \
                   FROM discord_user_handles h \
                   JOIN colleagues c ON c.id = h.colleague_id \
                  WHERE h.org_id = $1 AND h.discord_user_id = $2",
            )
            .bind(org_id)
            .bind(&discord_user)
            .fetch_optional(&mut **tx)
            .await?;

            if let Some((colleague_id, shadow_user)) = existing {
                if let Some(name) = display_name.as_deref() {
                    // Refresh the handle's observed display (a nick can change)…
                    sqlx::query(
                        "UPDATE discord_user_handles SET display_name = $3 \
                          WHERE org_id = $1 AND discord_user_id = $2 \
                            AND display_name IS DISTINCT FROM $3",
                    )
                    .bind(org_id)
                    .bind(&discord_user)
                    .bind(name)
                    .execute(&mut **tx)
                    .await?;
                    // …and backfill the synthetic user's name if still unset, so
                    // the agent's roster shows the real name, not the email.
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

            // 2. Mint the synthetic user (display-only, no `user_identities`).
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

            // 3. Grant org membership → fires `org_members_mint_colleague`.
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

            // 5. Record the handle. ON CONFLICT keeps the mint idempotent under a
            //    concurrent first-sight race (refresh the display name).
            sqlx::query(
                "INSERT INTO discord_user_handles \
                   (org_id, discord_user_id, display_name, colleague_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (org_id, discord_user_id) DO UPDATE SET \
                   display_name = EXCLUDED.display_name",
            )
            .bind(org_id)
            .bind(&discord_user)
            .bind(display_name.as_deref())
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

    async fn snowflake_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<DiscordUserId>, DiscordError> {
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, DiscordError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT discord_user_id FROM discord_user_handles \
                      WHERE org_id = $1 AND colleague_id = $2",
                )
                .bind(org_id)
                .bind(colleague)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        match row {
            Some((snowflake,)) => Ok(Some(DiscordUserId::try_from(snowflake)?)),
            None => Ok(None),
        }
    }

    async fn taggable_handles(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(String, DiscordUserId)>, DiscordError> {
        let cap = i64::try_from(DISCORD_TAG_HANDLES_MAX).unwrap_or(i64::MAX);
        let rows: Vec<(String, String)> =
            run_privileged::<Vec<(String, String)>, DiscordError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT COALESCE(u.display_name, split_part(u.email, '@', 1)) AS name, \
                            h.discord_user_id \
                       FROM discord_user_handles h \
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
        for (name, snowflake) in rows {
            out.push((name, DiscordUserId::try_from(snowflake)?));
        }
        Ok(out)
    }

    async fn tag_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<(String, DiscordUserId)>, DiscordError> {
        let row: Option<(String, String)> =
            run_privileged::<Option<(String, String)>, DiscordError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT COALESCE(u.display_name, split_part(u.email, '@', 1)) AS name, \
                            h.discord_user_id \
                       FROM discord_user_handles h \
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
            Some((name, snowflake)) => Ok(Some((name, DiscordUserId::try_from(snowflake)?))),
            None => Ok(None),
        }
    }

    async fn agent_name_for(
        &self,
        org_id: OrgId,
        colleague: ColleagueId,
    ) -> Result<Option<String>, DiscordError> {
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, DiscordError>(&self.pool, async move |tx| {
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
