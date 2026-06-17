//! Discord agent↔human DM binding — the `discord_dms` table (issue #178).
//!
//! A DM channel is opened on demand and is not a guild container, so it can't
//! live in `discord_threads`. This store maps a Patom DM thread to the bot's DM
//! channel snowflake: the router looks up by `patom_thread_id` before opening a
//! new channel (idempotency — a re-fire reuses the bound channel).
//!
//! Privileged throughout: the router holds no `Caller`; the org comes from the
//! resolved app registration.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::threads::ThreadId;

use super::error::DiscordError;
use super::types::{ApplicationId, ContainerId};

/// Reverse projection used by the outbound router: which bot + DM channel a
/// Patom DM thread is bound to.
#[derive(Debug, Clone)]
pub struct DiscordDmBinding {
    pub application_id: ApplicationId,
    pub dm_channel_id: ContainerId,
}

#[async_trait]
pub trait DiscordDmStore: fmt::Debug + Send + Sync {
    /// The DM channel a Patom thread is bound to, or `None` if it has no Discord
    /// DM binding yet.
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<DiscordDmBinding>, DiscordError>;

    /// Bind a Patom DM thread to a bot's DM channel. Idempotent on the
    /// `patom_thread_id` unique index — a concurrent / repeated bind is a no-op.
    async fn bind(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        patom_thread_id: ThreadId,
        dm_channel_id: &ContainerId,
    ) -> Result<(), DiscordError>;
}

pub type SharedDiscordDmStore = Arc<dyn DiscordDmStore>;

pub struct PgDiscordDmStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgDiscordDmStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgDiscordDmStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDiscordDmStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl DiscordDmStore for PgDiscordDmStore {
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<DiscordDmBinding>, DiscordError> {
        type Row = (String, String);
        let row: Option<Row> =
            run_privileged::<Option<Row>, DiscordError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT application_id, dm_channel_id \
                     FROM discord_dms WHERE patom_thread_id = $1",
                )
                .bind(thread_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        let Some((application_id, dm_channel_id)) = row else {
            return Ok(None);
        };
        Ok(Some(DiscordDmBinding {
            application_id: ApplicationId::try_from(application_id)?,
            dm_channel_id: ContainerId::try_from(dm_channel_id)?,
        }))
    }

    async fn bind(
        &self,
        org_id: OrgId,
        application_id: &ApplicationId,
        patom_thread_id: ThreadId,
        dm_channel_id: &ContainerId,
    ) -> Result<(), DiscordError> {
        let now = self.clock.now_utc();
        run_privileged::<(), DiscordError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO discord_dms \
                   (org_id, application_id, patom_thread_id, dm_channel_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (patom_thread_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(application_id.as_str())
            .bind(patom_thread_id)
            .bind(dm_channel_id.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
