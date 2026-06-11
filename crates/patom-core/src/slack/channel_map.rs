//! Slack channel ↔ Patom channel mapping — `slack_channels` table.
//!
//! A Slack-rooted conversation is a normal multi-participant **channel
//! thread**, not a two-party DM (issue #41). To get there, each Slack
//! channel is mirrored to a Patom `channels` row: the bridge
//! get-or-creates the mapping on the first inbound event and adds each
//! acting linked human to the Patom channel's `channel_members`, so a
//! second human in the same Slack thread can read and post (the DM
//! visibility predicate would have excluded them).
//!
//! Privileged throughout: the webhook carries no Principal, and the org
//! comes from the resolved workspace, not a session.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, UserId, run_privileged};
use crate::channels::{ChannelId, ChannelName};
use crate::clock::SharedClock;

use super::error::SlackError;
use super::types::{SlackChannelId, SlackTeamId};

#[async_trait]
pub trait SlackChannelStore: fmt::Debug + Send + Sync {
    /// Get-or-create the Patom channel mirroring a Slack channel, and
    /// ensure `user_id` is a member of it. Returns the Patom
    /// [`ChannelId`]. Idempotent: repeated calls return the same channel
    /// and only top up membership.
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        slack_channel_id: &SlackChannelId,
        user_id: UserId,
    ) -> Result<ChannelId, SlackError>;
}

pub type SharedSlackChannelStore = Arc<dyn SlackChannelStore>;

pub struct PgSlackChannelStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgSlackChannelStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgSlackChannelStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSlackChannelStore")
            .finish_non_exhaustive()
    }
}

/// Derive a valid Patom channel slug from a Slack channel id. Slack ids
/// are alphanumeric, so `"slack-" + lowercase(id)` always satisfies the
/// `^[a-z0-9][a-z0-9-]{0,62}$` `ChannelName` rule (and stays ≤ 63 bytes,
/// since a Slack id is ≤ 32 chars). A future enhancement could fetch the
/// human channel name via `conversations.info` (needs `channels:read`).
fn channel_name_for(slack_channel_id: &SlackChannelId) -> Result<ChannelName, SlackError> {
    let raw = format!("slack-{}", slack_channel_id.as_str());
    Ok(ChannelName::try_from(raw.as_str())?)
}

#[async_trait]
impl SlackChannelStore for PgSlackChannelStore {
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        slack_channel_id: &SlackChannelId,
        user_id: UserId,
    ) -> Result<ChannelId, SlackError> {
        let now = self.clock.now_utc();
        let team = team_id.as_str().to_owned();
        let slack_chan = slack_channel_id.as_str().to_owned();
        let name = channel_name_for(slack_channel_id)?;
        run_privileged::<ChannelId, SlackError>(&self.pool, async move |tx| {
            let existing: Option<(ChannelId,)> = sqlx::query_as(
                "SELECT channel_id FROM slack_channels \
                 WHERE org_id = $1 AND team_id = $2 AND slack_channel_id = $3",
            )
            .bind(org_id)
            .bind(&team)
            .bind(&slack_chan)
            .fetch_optional(&mut **tx)
            .await?;

            let channel_id = match existing {
                Some((c,)) => c,
                None => {
                    create_channel_and_mapping(tx, org_id, &team, &slack_chan, &name, now).await?
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

/// Create the Patom channel (idempotent on the active-name unique index)
/// and the `slack_channels` mapping, returning the authoritative mapped
/// `ChannelId`. The final re-read of the mapping is the source of truth so
/// a concurrent first-touch that won the mapping PK resolves to one
/// channel for both callers.
async fn create_channel_and_mapping(
    tx: &mut crate::auth::PrivilegedTx<'_>,
    org_id: OrgId,
    team: &str,
    slack_chan: &str,
    name: &ChannelName,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ChannelId, SlackError> {
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
        "INSERT INTO slack_channels (org_id, team_id, slack_channel_id, channel_id, created_at) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id, team_id, slack_channel_id) DO NOTHING",
    )
    .bind(org_id)
    .bind(team)
    .bind(slack_chan)
    .bind(cid)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let (mapped,): (ChannelId,) = sqlx::query_as(
        "SELECT channel_id FROM slack_channels \
         WHERE org_id = $1 AND team_id = $2 AND slack_channel_id = $3",
    )
    .bind(org_id)
    .bind(team)
    .bind(slack_chan)
    .fetch_one(&mut **tx)
    .await?;
    Ok(mapped)
}
