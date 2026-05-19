//! Slack-thread ↔ Relay-DAG bridge — `slack_threads` table.
//!
//! Two lookups, both privileged (no Principal — events arrive
//! workspace-keyed):
//! - `lookup_by_thread` for the inbound path: given a Slack
//!   `(team, channel, thread_ts)` triple, find an existing session or
//!   return `None` (caller starts a fresh session).
//! - `lookup_by_root` for the outbound stream pump: given a Relay
//!   `root_request_id`, find where to post.
//!
//! Writes happen via `bind_root` after `queue.enqueue_for_user` returns
//! the freshly minted `(session_id, root_request_id)` for a new thread.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::runtime::PromptRequestId;
use crate::session::SessionId;

use super::error::SlackError;
use super::types::{SlackChannelId, SlackTeamId, SlackThreadTs};

/// Existing Slack-thread → Relay-session mapping.
#[derive(Debug, Clone, Copy)]
pub struct ThreadMapping {
    pub org_id: OrgId,
    pub session_id: SessionId,
    pub root_request_id: PromptRequestId,
}

/// Reverse projection used by the stream pump.
#[derive(Debug, Clone)]
pub struct ThreadByRoot {
    pub org_id: OrgId,
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub thread_ts: SlackThreadTs,
}

#[async_trait]
pub trait SlackThreadStore: fmt::Debug + Send + Sync {
    async fn lookup_by_thread(
        &self,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
    ) -> Result<Option<ThreadMapping>, SlackError>;

    async fn lookup_by_root(
        &self,
        root: PromptRequestId,
    ) -> Result<Option<ThreadByRoot>, SlackError>;

    /// Insert a `(team, channel, thread_ts) → (session, root)` row.
    /// Idempotent on the primary key; an existing row is left alone
    /// (the second mention in a thread should still resolve to the
    /// existing session).
    async fn bind_root(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
        session_id: SessionId,
        root_request_id: PromptRequestId,
    ) -> Result<(), SlackError>;
}

pub type SharedSlackThreadStore = Arc<dyn SlackThreadStore>;

pub struct PgSlackThreadStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgSlackThreadStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgSlackThreadStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSlackThreadStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl SlackThreadStore for PgSlackThreadStore {
    async fn lookup_by_thread(
        &self,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
    ) -> Result<Option<ThreadMapping>, SlackError> {
        type Row = (OrgId, SessionId, PromptRequestId);
        let row: Option<Row> = run_privileged::<Option<Row>, SlackError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, session_id, root_request_id \
                 FROM slack_threads \
                 WHERE team_id = $1 AND channel_id = $2 AND thread_ts = $3",
            )
            .bind(team_id.as_str())
            .bind(channel_id.as_str())
            .bind(thread_ts.as_str())
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        Ok(
            row.map(|(org_id, session_id, root_request_id)| ThreadMapping {
                org_id,
                session_id,
                root_request_id,
            }),
        )
    }

    async fn lookup_by_root(
        &self,
        root: PromptRequestId,
    ) -> Result<Option<ThreadByRoot>, SlackError> {
        type Row = (OrgId, String, String, String);
        let row: Option<Row> = run_privileged::<Option<Row>, SlackError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, team_id, channel_id, thread_ts \
                 FROM slack_threads WHERE root_request_id = $1",
            )
            .bind(root)
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let Some((org_id, team_id_str, channel_id_str, thread_ts_str)) = row else {
            return Ok(None);
        };
        Ok(Some(ThreadByRoot {
            org_id,
            team_id: SlackTeamId::try_from(team_id_str)?,
            channel_id: SlackChannelId::try_from(channel_id_str)?,
            thread_ts: SlackThreadTs::try_from(thread_ts_str)?,
        }))
    }

    async fn bind_root(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
        session_id: SessionId,
        root_request_id: PromptRequestId,
    ) -> Result<(), SlackError> {
        let now = self.clock.now_utc();
        run_privileged::<(), SlackError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO slack_threads \
                   (org_id, team_id, channel_id, thread_ts, root_request_id, session_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (team_id, channel_id, thread_ts) DO NOTHING",
            )
            .bind(org_id)
            .bind(team_id.as_str())
            .bind(channel_id.as_str())
            .bind(thread_ts.as_str())
            .bind(root_request_id)
            .bind(session_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
