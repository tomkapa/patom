//! Slack-thread ↔ Patom-thread bridge — `slack_threads` table.
//!
//! One row per Slack-rooted conversation: a Slack `(team, channel, thread_ts)`
//! triple ↔ one Patom `thread_id`. Both lookups are privileged (no Principal —
//! events arrive workspace-keyed):
//! - `lookup_by_thread` for the inbound path: given a Slack `(team, channel,
//!   thread_ts)` triple, find the bound Patom thread or `None` (caller starts a
//!   fresh thread).
//! - `lookup_by_thread_id` for the outbound stream pump: given a Patom
//!   `thread_id`, find which Slack thread (if any) it is bound to.
//!
//! Writes happen via `bind`: the inbound bridge writes after it creates (or
//! resolves) the Patom thread for the first mention in a Slack thread.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::threads::ThreadId;

use super::error::SlackError;
use super::types::{SlackChannelId, SlackTeamId, SlackThreadTs};

/// Existing Slack-thread → Patom-thread mapping.
#[derive(Debug, Clone, Copy)]
pub struct ThreadMapping {
    pub org_id: OrgId,
    pub thread_id: ThreadId,
}

/// Reverse projection used by the stream pump: where a Patom thread's chunks
/// should land in Slack.
#[derive(Debug, Clone)]
pub struct ThreadByThreadId {
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

    /// Reverse projection used by the outbound stream pump: given a
    /// `thread_id`, return the Slack `(team, channel, thread_ts)` where the
    /// thread's chunks should land — or `None` if this thread has no Slack
    /// binding (it originated on the web, not in Slack).
    async fn lookup_by_thread_id(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadByThreadId>, SlackError>;

    /// Insert a `(team, channel, thread_ts) → thread_id` row. Idempotent on the
    /// primary key; an existing row is left alone (the second mention in a Slack
    /// thread should still resolve to the existing Patom thread).
    async fn bind(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
        thread_id: ThreadId,
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
        type Row = (OrgId, ThreadId);
        let row: Option<Row> = run_privileged::<Option<Row>, SlackError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, thread_id \
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
        Ok(row.map(|(org_id, thread_id)| ThreadMapping { org_id, thread_id }))
    }

    async fn lookup_by_thread_id(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadByThreadId>, SlackError> {
        type Row = (OrgId, String, String, String);
        let row: Option<Row> = run_privileged::<Option<Row>, SlackError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, team_id, channel_id, thread_ts \
                 FROM slack_threads WHERE thread_id = $1",
            )
            .bind(thread_id)
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let Some((org_id, team_id_str, channel_id_str, thread_ts_str)) = row else {
            return Ok(None);
        };
        Ok(Some(ThreadByThreadId {
            org_id,
            team_id: SlackTeamId::try_from(team_id_str)?,
            channel_id: SlackChannelId::try_from(channel_id_str)?,
            thread_ts: SlackThreadTs::try_from(thread_ts_str)?,
        }))
    }

    async fn bind(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
        thread_id: ThreadId,
    ) -> Result<(), SlackError> {
        let now = self.clock.now_utc();
        run_privileged::<(), SlackError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO slack_threads \
                   (org_id, team_id, channel_id, thread_ts, thread_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (team_id, channel_id, thread_ts) DO NOTHING",
            )
            .bind(org_id)
            .bind(team_id.as_str())
            .bind(channel_id.as_str())
            .bind(thread_ts.as_str())
            .bind(thread_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}

/// In-memory `SlackThreadStore` for tests. Records writes and answers
/// reads without touching Postgres. Not `#[cfg(test)]` so integration
/// tests in `tests/` can reach it.
#[derive(Debug, Default)]
pub struct FakeSlackThreadStore {
    inner: std::sync::Mutex<FakeInner>,
}

#[derive(Debug, Default)]
struct FakeInner {
    by_thread: std::collections::HashMap<ThreadKey, ThreadMapping>,
    by_thread_id: std::collections::HashMap<ThreadId, ThreadByThreadId>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ThreadKey {
    team_id: SlackTeamId,
    channel_id: SlackChannelId,
    thread_ts: SlackThreadTs,
}

impl FakeSlackThreadStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of bindings currently recorded.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned")
            .by_thread
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SlackThreadStore for FakeSlackThreadStore {
    async fn lookup_by_thread(
        &self,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
    ) -> Result<Option<ThreadMapping>, SlackError> {
        let key = ThreadKey {
            team_id: team_id.clone(),
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        };
        let guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        Ok(guard.by_thread.get(&key).copied())
    }

    async fn lookup_by_thread_id(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadByThreadId>, SlackError> {
        let guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        Ok(guard.by_thread_id.get(&thread_id).cloned())
    }

    async fn bind(
        &self,
        org_id: OrgId,
        team_id: &SlackTeamId,
        channel_id: &SlackChannelId,
        thread_ts: &SlackThreadTs,
        thread_id: ThreadId,
    ) -> Result<(), SlackError> {
        let mut guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        let key = ThreadKey {
            team_id: team_id.clone(),
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        };
        // PK conflict on (team, channel, thread_ts) — leave first writer.
        if guard.by_thread.contains_key(&key) {
            return Ok(());
        }
        // UNIQUE(thread_id) — mirrors the post-migration-64 schema.
        if guard.by_thread_id.contains_key(&thread_id) {
            return Err(SlackError::Internal(format!(
                "fake: duplicate binding for thread {thread_id}"
            )));
        }
        guard
            .by_thread
            .insert(key.clone(), ThreadMapping { org_id, thread_id });
        guard.by_thread_id.insert(
            thread_id,
            ThreadByThreadId {
                org_id,
                team_id: key.team_id,
                channel_id: key.channel_id,
                thread_ts: key.thread_ts,
            },
        );
        Ok(())
    }
}
