//! Discord-container ↔ Patom-thread bridge — `discord_threads` table.
//!
//! One row per Discord-rooted conversation: a `(guild_id, container_id)` pair ↔
//! one Patom `thread_id`. A Discord *thread* is a channel (same post endpoint),
//! so `container_id` is the channel id for a top-level channel or the thread id
//! for a thread. Both lookups are privileged (live events are app-keyed, not
//! caller-authenticated):
//! - `lookup_by_container` for the inbound path: find the bound Patom thread or
//!   `None` (the caller then creates one).
//! - `lookup_by_patom_thread` for the outbound stream pump: find which Discord
//!   container a Patom thread is bound to.
//!
//! `backfill_complete` gates the one-shot pre-join history backfill (§5) so it
//! runs at most once per container.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::threads::ThreadId;

use super::error::DiscordError;
use super::types::{ApplicationId, ContainerId, GuildId};

/// Existing Discord-container → Patom-thread mapping (inbound resolve).
#[derive(Debug, Clone)]
pub struct DiscordThreadMapping {
    pub thread_id: ThreadId,
    /// Whether the one-shot pre-join history backfill has run for this container.
    pub backfill_complete: bool,
    /// Whether this container is a thread the bot owns, derived from
    /// `parent_id IS NOT NULL` (a bot-opened thread binds with its parent
    /// channel; a top-level channel / DM binds with `parent_id = NULL`). The
    /// bridge uses it to avoid opening a thread inside a thread.
    pub is_thread: bool,
}

/// Reverse projection used by the stream pump: where a Patom thread's chunks
/// should land in Discord.
#[derive(Debug, Clone)]
pub struct DiscordThreadBinding {
    pub application_id: ApplicationId,
    pub guild_id: GuildId,
    pub container_id: ContainerId,
}

#[async_trait]
pub trait DiscordThreadStore: fmt::Debug + Send + Sync {
    /// Inbound resolve: given the bot (`org_id`, `application_id`) and
    /// `(guild_id, container_id)`, return the bound Patom thread or `None` (no
    /// binding yet). Scoped to the bot so two bots sharing a guild/channel — even
    /// across orgs — resolve their own bindings, not each other's.
    async fn lookup_by_container(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
    ) -> Result<Option<DiscordThreadMapping>, DiscordError>;

    /// Reverse projection used by the outbound pump: given a `thread_id`, return
    /// the Discord container its chunks should land in — or `None` if this thread
    /// has no Discord binding (it originated on the web).
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<DiscordThreadBinding>, DiscordError>;

    /// Insert a `(guild_id, container_id) → patom_thread_id` row. Idempotent on
    /// the primary key; an existing row is left alone so a later message in the
    /// same container still resolves to the existing Patom thread. `parent_id`
    /// is the thread's parent channel when known (NULL otherwise — a top-level
    /// channel, a DM, or a user-made thread whose parent we never learned).
    /// `is_thread` records whether the container is a thread (or otherwise
    /// non-threadable) so the bridge converses in it instead of re-attempting a
    /// thread-open — independent of whether `parent_id` is known.
    // The columns of one binding row; positional is clearer than a one-call-site
    // param struct here.
    #[allow(clippy::too_many_arguments)]
    async fn bind(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
        parent_id: Option<&ContainerId>,
        is_thread: bool,
        patom_thread_id: ThreadId,
    ) -> Result<(), DiscordError>;

    /// Mark a container's one-shot history backfill as complete. Idempotent.
    /// Scoped to the bot (`org_id`, `application_id`) so it updates this bot's
    /// binding, never another bot's row for the same container.
    async fn mark_backfilled(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
    ) -> Result<(), DiscordError>;
}

pub type SharedDiscordThreadStore = Arc<dyn DiscordThreadStore>;

/// Postgres-backed [`DiscordThreadStore`]. All methods run privileged: live
/// Discord events are app-keyed, not caller-authenticated.
pub struct PgDiscordThreadStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgDiscordThreadStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgDiscordThreadStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDiscordThreadStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DiscordThreadStore for PgDiscordThreadStore {
    async fn lookup_by_container(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
    ) -> Result<Option<DiscordThreadMapping>, DiscordError> {
        type Row = (ThreadId, bool, bool);
        let row: Option<Row> =
            run_privileged::<Option<Row>, DiscordError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT patom_thread_id, backfill_complete, is_thread \
                     FROM discord_threads \
                     WHERE org_id = $1 AND application_id = $2 \
                       AND guild_id = $3 AND container_id = $4",
                )
                .bind(org_id)
                .bind(application_id.as_str())
                .bind(guild_id.as_str())
                .bind(container_id.as_str())
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.map(
            |(thread_id, backfill_complete, is_thread)| DiscordThreadMapping {
                thread_id,
                backfill_complete,
                is_thread,
            },
        ))
    }

    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<DiscordThreadBinding>, DiscordError> {
        type Row = (String, String, String);
        let row: Option<Row> =
            run_privileged::<Option<Row>, DiscordError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT application_id, guild_id, container_id \
                     FROM discord_threads WHERE patom_thread_id = $1",
                )
                .bind(thread_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        let Some((application_id, guild_id, container_id)) = row else {
            return Ok(None);
        };
        Ok(Some(DiscordThreadBinding {
            application_id: ApplicationId::try_from(application_id)?,
            guild_id: GuildId::try_from(guild_id)?,
            container_id: ContainerId::try_from(container_id)?,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn bind(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
        parent_id: Option<&ContainerId>,
        is_thread: bool,
        patom_thread_id: ThreadId,
    ) -> Result<(), DiscordError> {
        let now = self.clock.now_utc();
        run_privileged::<(), DiscordError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO discord_threads \
                   (org_id, application_id, guild_id, container_id, parent_id, is_thread, patom_thread_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (org_id, application_id, guild_id, container_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(application_id.as_str())
            .bind(guild_id.as_str())
            .bind(container_id.as_str())
            .bind(parent_id.map(ContainerId::as_str))
            .bind(is_thread)
            .bind(patom_thread_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn mark_backfilled(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        guild_id: &GuildId,
        container_id: &ContainerId,
    ) -> Result<(), DiscordError> {
        run_privileged::<(), DiscordError>(&self.pool, async |tx| {
            sqlx::query(
                "UPDATE discord_threads SET backfill_complete = TRUE \
                 WHERE org_id = $1 AND application_id = $2 \
                   AND guild_id = $3 AND container_id = $4",
            )
            .bind(org_id)
            .bind(application_id.as_str())
            .bind(guild_id.as_str())
            .bind(container_id.as_str())
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
