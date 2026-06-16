//! Discord channel ↔ Patom channel mapping — `discord_channels` table.
//!
//! A Discord-rooted conversation is a normal multi-participant **channel
//! thread**, not a two-party DM. Each Discord container (a top-level channel, or
//! a thread — which is itself a channel) is mirrored to a Patom `channels` row:
//! the bridge get-or-creates the mapping on the first inbound event and adds each
//! acting shadow human to `channel_members`, so a second human in the same
//! Discord channel can read and post (the DM visibility predicate would have
//! excluded them).
//!
//! Privileged throughout: the gateway event carries no Principal, and the org
//! comes from the resolved `discord_apps` registration.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, UserId, run_privileged};
use crate::channels::{ChannelId, ChannelName};
use crate::clock::SharedClock;

use super::error::DiscordError;
use super::types::{ContainerId, GuildId};

#[async_trait]
pub trait DiscordChannelStore: fmt::Debug + Send + Sync {
    /// Get-or-create the Patom channel mirroring a Discord channel, and ensure
    /// `user_id` is a member of it. Idempotent: repeated calls return the same
    /// channel and only top up membership.
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        guild_id: &GuildId,
        discord_channel_id: &ContainerId,
        user_id: UserId,
    ) -> Result<ChannelId, DiscordError>;
}

pub type SharedDiscordChannelStore = Arc<dyn DiscordChannelStore>;

pub struct PgDiscordChannelStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgDiscordChannelStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgDiscordChannelStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDiscordChannelStore")
            .finish_non_exhaustive()
    }
}

/// Derive a valid Patom channel slug from a Discord channel snowflake.
///
/// Snowflakes are decimal digits and the `ChannelName` rule is
/// `^[a-z0-9][a-z0-9-]{0,62}$`, so `discord-<snowflake>` is always valid
/// (well under the 63-byte cap). A future enhancement could fetch the human
/// channel name via the REST API.
fn channel_name_for(discord_channel_id: &ContainerId) -> Result<ChannelName, DiscordError> {
    let raw = format!("discord-{}", discord_channel_id.as_str());
    Ok(ChannelName::try_from(raw.as_str())?)
}

#[async_trait]
impl DiscordChannelStore for PgDiscordChannelStore {
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        guild_id: &GuildId,
        discord_channel_id: &ContainerId,
        user_id: UserId,
    ) -> Result<ChannelId, DiscordError> {
        let now = self.clock.now_utc();
        let guild = guild_id.as_str().to_owned();
        let channel = discord_channel_id.as_str().to_owned();
        let name = channel_name_for(discord_channel_id)?;
        run_privileged::<ChannelId, DiscordError>(&self.pool, async move |tx| {
            let existing: Option<(ChannelId,)> = sqlx::query_as(
                "SELECT channel_id FROM discord_channels \
                 WHERE org_id = $1 AND guild_id = $2 AND discord_channel_id = $3",
            )
            .bind(org_id)
            .bind(&guild)
            .bind(&channel)
            .fetch_optional(&mut **tx)
            .await?;

            let channel_id = match existing {
                Some((c,)) => c,
                None => {
                    create_channel_and_mapping(tx, org_id, &guild, &channel, &name, now).await?
                }
            };

            // Top up membership so the acting human can read + post in the
            // channel thread (channel-membership is the visibility gate).
            sqlx::query(
                "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (channel_id, user_id) DO NOTHING",
            )
            .bind(channel_id)
            .bind(user_id)
            .bind(org_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(channel_id)
        })
        .await
    }
}

/// Create the Patom channel and the `discord_channels` mapping. Idempotent on
/// the active-name unique index; returns the channel id.
async fn create_channel_and_mapping(
    tx: &mut crate::auth::PrivilegedTx<'_>,
    org_id: OrgId,
    guild: &str,
    channel: &str,
    name: &ChannelName,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ChannelId, DiscordError> {
    let new_id = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, $3, NULL, $4) \
         ON CONFLICT (org_id, name) WHERE archived_at IS NULL DO NOTHING",
    )
    .bind(new_id)
    .bind(org_id)
    .bind(name.as_str())
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let (cid,): (ChannelId,) = sqlx::query_as(
        "SELECT id FROM channels WHERE org_id = $1 AND name = $2 AND archived_at IS NULL",
    )
    .bind(org_id)
    .bind(name.as_str())
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO discord_channels (org_id, guild_id, discord_channel_id, channel_id, created_at) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id, guild_id, discord_channel_id) DO NOTHING",
    )
    .bind(org_id)
    .bind(guild)
    .bind(channel)
    .bind(cid)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(cid)
}
