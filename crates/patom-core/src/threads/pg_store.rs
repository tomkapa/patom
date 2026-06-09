//! Postgres-backed [`ThreadStore`].
//!
//! Feed rows live in `thread_messages (thread_id, seq, kind, sender_colleague_id,
//! owner_agent_id, receiver_colleague_id, body JSONB, request_id, …)`. Per-thread
//! ordering is the `thread_seq` counter, bumped atomically inside `append`. Wall
//! clock comes from the injected [`SharedClock`] (CLAUDE.md §11).

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{Caller, UserId, run_as_user, run_privileged};
use crate::channels::ChannelId;
use crate::clock::SharedClock;
use crate::colleagues::{ColleagueId, ColleagueKind};
use crate::provider::{AssistantContent, ChatMessage, UserContent};
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::ThreadError;
use super::limits::{MAX_THREAD_FEED, MAX_THREAD_LIST};
use super::traits::{
    AgentThreadId, FeedMessage, MessageKind, NewMessage, ThreadId, ThreadListItem, ThreadMessageId,
    ThreadStore,
};

/// Postgres-backed [`ThreadStore`].
pub struct PgThreadStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgThreadStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgThreadStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgThreadStore").finish_non_exhaustive()
    }
}

/// Context read: a `posted` row from anyone, or one of `agent`'s own private
/// artifacts. `$1` thread, `$2` agent. Ordered by the per-thread feed seq.
const CONTEXT_SQL: &str = "SELECT m.kind, m.sender_colleague_id, m.body \
     FROM thread_messages m \
     WHERE m.thread_id = $1 AND (m.kind = 'posted' OR m.owner_agent_id = $2) \
     ORDER BY m.seq ASC";

/// G2 flat-feed read. Both participant sides joined to `colleagues` for their
/// satellite columns (kind / user_id / agent_id) so the HTTP boundary decodes a
/// `Participant`/`MessageSender` and enriches a human name/avatar. The
/// visibility gate mirrors `list_threads` (channel membership, or DM ownership)
/// and pins the active org (`$2`). Pages backward on the `seq` keyset
/// (`$4`); ordered DESC for the LIMIT then reversed to ascending by the caller.
const FEED_SQL: &str = "SELECT m.seq, m.kind, \
        m.sender_colleague_id, sc.kind, sc.user_id, sc.agent_id, \
        m.owner_agent_id, \
        m.receiver_colleague_id, rc.kind, rc.user_id, rc.agent_id, \
        m.body, m.request_id, m.created_at \
     FROM thread_messages m \
     JOIN threads t ON t.id = m.thread_id \
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
     LEFT JOIN colleagues rc ON rc.id = m.receiver_colleague_id \
     WHERE m.thread_id = $1 AND t.org_id = $2 \
       AND (CASE WHEN t.channel_id IS NULL THEN \
                EXISTS (SELECT 1 FROM colleagues cb \
                        WHERE cb.id = t.created_by_colleague_id AND cb.user_id = $3) \
            ELSE \
                EXISTS (SELECT 1 FROM channel_members cm \
                        WHERE cm.channel_id = t.channel_id AND cm.user_id = $3) \
                AND EXISTS (SELECT 1 FROM channels c \
                            WHERE c.id = t.channel_id AND c.archived_at IS NULL) \
            END) \
       AND ($4::bigint IS NULL OR m.seq < $4) \
     ORDER BY m.seq DESC \
     LIMIT $5";

#[async_trait]
impl ThreadStore for PgThreadStore {
    #[tracing::instrument(skip_all, name = "thread.create", fields(patom.thread.id = tracing::field::Empty))]
    async fn create_thread(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
        root_message_id: Option<ThreadMessageId>,
        created_by: ColleagueId,
    ) -> Result<ThreadId, ThreadError> {
        let now = self.now();
        let id = ThreadId::new();
        run_as_user(&self.pool, caller.user_id, async |tx| {
            sqlx::query(
                "INSERT INTO threads \
                   (id, org_id, channel_id, root_message_id, created_by_colleague_id, \
                    created_at, last_activity_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6)",
            )
            .bind(id)
            .bind(caller.org_id)
            .bind(channel_id)
            .bind(root_message_id)
            .bind(created_by)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok::<(), ThreadError>(())
        })
        .await?;
        tracing::Span::current().record("patom.thread.id", tracing::field::display(id));
        Ok(id)
    }

    #[tracing::instrument(skip_all, name = "thread.resolve_participation", fields(patom.thread.id = %thread, patom.agent.id = %agent))]
    async fn resolve_participation(
        &self,
        caller: &Caller,
        thread: ThreadId,
        agent: AgentId,
    ) -> Result<AgentThreadId, ThreadError> {
        let now = self.now();
        let new_id = AgentThreadId::new();
        let row: Option<(AgentThreadId,)> = run_as_user::<Option<(AgentThreadId,)>, ThreadError>(
            &self.pool,
            caller.user_id,
            async |tx| {
                Ok(sqlx::query_as(
                    "INSERT INTO agent_thread_state (id, thread_id, agent_id, org_id, created_at) \
                 SELECT $1, $2, $3, t.org_id, $4 FROM threads t WHERE t.id = $2 \
                 ON CONFLICT (thread_id, agent_id) DO UPDATE SET id = agent_thread_state.id \
                 RETURNING id",
                )
                .bind(new_id)
                .bind(thread)
                .bind(agent)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(|(id,)| id).ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.append", fields(patom.thread.id = %thread, patom.message.kind = message.kind.as_str()))]
    async fn append(
        &self,
        caller: &Caller,
        thread: ThreadId,
        message: NewMessage,
    ) -> Result<ThreadMessageId, ThreadError> {
        let now = self.now();
        let body = serde_json::to_value(&message.body)
            .map_err(|e| ThreadError::Backend(format!("serialize message: {e}")))?;
        let id = ThreadMessageId::new();
        // One round trip: bump the per-thread seq, insert the row at that seq,
        // and bump last_activity_at — all gated on the thread existing (`t`).
        let row: Option<(i64,)> =
            run_as_user::<Option<(i64,)>, ThreadError>(&self.pool, caller.user_id, async |tx| {
                Ok(sqlx::query_as(
                    "WITH t AS (SELECT org_id FROM threads WHERE id = $1), \
                 seqx AS ( \
                     INSERT INTO thread_seq (thread_id, next_seq, org_id) \
                     SELECT $1, 1, t.org_id FROM t \
                     ON CONFLICT (thread_id) DO UPDATE SET next_seq = thread_seq.next_seq + 1 \
                     RETURNING next_seq \
                 ), \
                 ins AS ( \
                     INSERT INTO thread_messages \
                         (id, thread_id, seq, kind, sender_colleague_id, owner_agent_id, \
                          receiver_colleague_id, body, request_id, org_id, created_at) \
                     SELECT $2, $1, seqx.next_seq, $3, $4, $5, $6, $7, $8, t.org_id, $9 \
                     FROM seqx, t \
                     RETURNING seq \
                 ), \
                 upd AS ( \
                     UPDATE threads SET last_activity_at = $9 \
                     WHERE id = $1 AND EXISTS (SELECT 1 FROM ins) \
                 ) \
                 SELECT seq FROM ins",
                )
                .bind(thread)
                .bind(id)
                .bind(message.kind)
                .bind(message.sender)
                .bind(message.owner_agent_id)
                .bind(message.receiver)
                .bind(body)
                .bind(message.request_id)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        // The `seq` row confirms the insert fired (thread exists); we return the
        // surface id we minted, which callers thread into reply-roots / triggers.
        row.map(|(_seq,)| id).ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.list_threads", fields(patom.org.id = %caller.org_id, patom.channel.id = ?channel_id, patom.thread.count = tracing::field::Empty))]
    async fn list_threads(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
    ) -> Result<Vec<ThreadListItem>, ThreadError> {
        type Row = (ThreadId, Option<ChannelId>, DateTime<Utc>);
        let org = caller.org_id;
        let user = caller.user_id;
        // Channel view: gated on the caller's membership + channel not archived
        // (visible to every member, not just the creator — P7). DM view: the
        // caller's own channel-less threads. Org-pinned so a multi-org member's
        // other workspaces never leak in (RLS gates membership, not active org).
        let rows: Vec<Row> =
            run_as_user::<Vec<Row>, ThreadError>(&self.pool, user, async |tx| match channel_id {
                Some(channel) => Ok(sqlx::query_as(
                    "SELECT t.id, t.channel_id, t.last_activity_at FROM threads t \
                     WHERE t.org_id = $1 AND t.channel_id = $2 \
                       AND EXISTS (SELECT 1 FROM channel_members cm \
                                   WHERE cm.channel_id = t.channel_id AND cm.user_id = $3) \
                       AND EXISTS (SELECT 1 FROM channels c \
                                   WHERE c.id = t.channel_id AND c.archived_at IS NULL) \
                     ORDER BY t.last_activity_at DESC LIMIT $4",
                )
                .bind(org)
                .bind(channel)
                .bind(user)
                .bind(MAX_THREAD_LIST)
                .fetch_all(&mut **tx)
                .await?),
                None => Ok(sqlx::query_as(
                    "SELECT t.id, t.channel_id, t.last_activity_at FROM threads t \
                     JOIN colleagues cb ON cb.id = t.created_by_colleague_id \
                     WHERE t.org_id = $1 AND t.channel_id IS NULL AND cb.user_id = $2 \
                     ORDER BY t.last_activity_at DESC LIMIT $3",
                )
                .bind(org)
                .bind(user)
                .bind(MAX_THREAD_LIST)
                .fetch_all(&mut **tx)
                .await?),
            })
            .await?;
        tracing::Span::current().record("patom.thread.count", rows.len());
        Ok(rows
            .into_iter()
            .map(|(thread_id, channel_id, last_activity_at)| ThreadListItem {
                thread_id,
                channel_id,
                last_activity_at,
            })
            .collect())
    }

    #[tracing::instrument(skip_all, name = "thread.is_channel_member", fields(patom.thread.id = %thread, patom.user.id = %user_id))]
    async fn is_channel_member(
        &self,
        thread: ThreadId,
        user_id: crate::auth::UserId,
    ) -> Result<bool, ThreadError> {
        // One round trip: resolve the thread's channel and, for a channel
        // thread, test membership; a DM thread (NULL channel) is always
        // reachable by its human. Privileged — the agent is org-global and the
        // (channel, user) pair is fully qualified, so no cross-org leak.
        let row: Option<(bool,)> =
            run_privileged::<Option<(bool,)>, ThreadError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT CASE \
                         WHEN t.channel_id IS NULL THEN true \
                         ELSE EXISTS ( \
                             SELECT 1 FROM channel_members m \
                             WHERE m.channel_id = t.channel_id AND m.user_id = $2 \
                         ) END \
                     FROM threads t WHERE t.id = $1",
                )
                .bind(thread)
                .bind(user_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        row.map(|(ok,)| ok).ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.feed", fields(patom.thread.id = %thread, patom.feed.count = tracing::field::Empty))]
    async fn feed(
        &self,
        caller: &Caller,
        thread: ThreadId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<FeedMessage>, ThreadError> {
        let cap = i64::from(limit.clamp(1, u32::try_from(MAX_THREAD_FEED).unwrap_or(u32::MAX)));
        let org = caller.org_id;
        let user = caller.user_id;
        let mut rows: Vec<FeedRow> =
            run_as_user::<Vec<FeedRow>, ThreadError>(&self.pool, user, async |tx| {
                Ok(sqlx::query_as(FEED_SQL)
                    .bind(thread)
                    .bind(org)
                    .bind(user)
                    .bind(before_seq)
                    .bind(cap)
                    .fetch_all(&mut **tx)
                    .await?)
            })
            .await?;
        // §5: the LIMIT bounds the batch; assert it held so a query change that
        // drops the LIMIT trips here rather than shipping an unbounded page.
        assert!(
            i64::try_from(rows.len()).unwrap_or(i64::MAX) <= cap,
            "invariant: feed respects its LIMIT"
        );
        // Query is DESC for the keyset LIMIT; the feed is displayed oldest→newest.
        rows.reverse();
        tracing::Span::current().record("patom.feed.count", rows.len());
        rows.into_iter().map(feed_row_to_message).collect()
    }

    #[tracing::instrument(skip_all, name = "thread.channel_of", fields(patom.thread.id = %thread))]
    async fn channel_of(&self, thread: ThreadId) -> Result<Option<ChannelId>, ThreadError> {
        // Privileged point lookup — the caller is an agent (org-global) reading
        // the location of a thread it is participating in. Distinguish a DM
        // thread (`channel_id IS NULL`) from a missing thread via `Option` on
        // the row itself.
        let row: Option<(Option<ChannelId>,)> =
            run_privileged::<Option<(Option<ChannelId>,)>, ThreadError>(&self.pool, async |tx| {
                Ok(
                    sqlx::query_as("SELECT channel_id FROM threads WHERE id = $1")
                        .bind(thread)
                        .fetch_optional(&mut **tx)
                        .await?,
                )
            })
            .await?;
        row.map(|(channel,)| channel)
            .ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.last_agent", fields(patom.thread.id = %thread))]
    async fn last_agent(&self, thread: ThreadId) -> Result<Option<AgentId>, ThreadError> {
        // Privileged point lookup — the Slack bridge is workspace-keyed infra
        // routing a reply to "the agent this thread is with". Most-recent
        // participation wins when a thread has gained more than one agent.
        let row: Option<(AgentId,)> =
            run_privileged::<Option<(AgentId,)>, ThreadError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT agent_id FROM agent_thread_state \
                     WHERE thread_id = $1 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(thread)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.map(|(agent,)| agent))
    }

    #[tracing::instrument(skip_all, name = "thread.visible_to", fields(patom.thread.id = %thread, patom.user.id = %caller.user_id))]
    async fn visible_to(&self, caller: &Caller, thread: ThreadId) -> Result<bool, ThreadError> {
        // Same visibility predicate as `feed`/`list_threads`, collapsed to an
        // existence test and run under the caller (RLS-scoped). A missing /
        // cross-org / non-member thread yields `false`, not an error.
        let org = caller.org_id;
        let user = caller.user_id;
        let row: (bool,) =
            run_as_user::<(bool,), ThreadError>(&self.pool, user, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT EXISTS( \
                         SELECT 1 FROM threads t \
                         WHERE t.id = $1 AND t.org_id = $2 \
                           AND (CASE WHEN t.channel_id IS NULL THEN \
                                    EXISTS (SELECT 1 FROM colleagues cb \
                                            WHERE cb.id = t.created_by_colleague_id AND cb.user_id = $3) \
                                ELSE \
                                    EXISTS (SELECT 1 FROM channel_members cm \
                                            WHERE cm.channel_id = t.channel_id AND cm.user_id = $3) \
                                    AND EXISTS (SELECT 1 FROM channels c \
                                                WHERE c.id = t.channel_id AND c.archived_at IS NULL) \
                                END))",
                )
                .bind(thread)
                .bind(org)
                .bind(user)
                .fetch_one(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.0)
    }

    #[tracing::instrument(skip_all, name = "thread.context_for_agent", fields(patom.thread.id = %thread, patom.agent.id = %agent, patom.history.count = tracing::field::Empty))]
    async fn context_for_agent(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
    ) -> Result<Vec<ChatMessage>, ThreadError> {
        type Row = (MessageKind, Option<ColleagueId>, serde_json::Value);
        let rows: Vec<Row> = run_privileged::<Vec<Row>, ThreadError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(CONTEXT_SQL)
                .bind(thread)
                .bind(agent)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;
        tracing::Span::current().record("patom.history.count", rows.len());

        let mut out = Vec::with_capacity(rows.len());
        for (kind, sender, body) in rows {
            let stored: ChatMessage = serde_json::from_value(body)
                .map_err(|e| ThreadError::Backend(format!("deserialize message: {e}")))?;
            out.push(map_row_for_viewer(kind, sender, stored, viewer));
        }
        Ok(out)
    }
}

/// One [`FEED_SQL`] row: seq, kind, the sender's colleague satellites, the
/// owner agent, the receiver's colleague satellites, body, request id, ts.
#[allow(clippy::type_complexity)]
type FeedRow = (
    i64,
    MessageKind,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    Option<AgentId>,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    serde_json::Value,
    Option<PromptRequestId>,
    DateTime<Utc>,
);

/// Decode one [`FeedRow`] into the public [`FeedMessage`], parsing both
/// participant sides once through the canonical `Participant::try_from` (§1)
/// rather than carrying raw columns to the boundary. A NULL sender colleague
/// decodes to [`Participant::System`] → [`MessageSender::System`]; a NULL
/// receiver colleague means the row addresses no one (`None`).
fn feed_row_to_message(row: FeedRow) -> Result<FeedMessage, ThreadError> {
    let (
        seq,
        kind,
        sender_colleague,
        sender_kind,
        sender_user,
        sender_agent,
        owner_agent_id,
        receiver_colleague,
        receiver_kind,
        receiver_user,
        receiver_agent,
        body,
        request_id,
        created_at,
    ) = row;
    let sender = MessageSender::from(decode_participant(
        sender_colleague,
        sender_kind,
        sender_user,
        sender_agent,
    )?);
    let receiver = match receiver_colleague {
        Some(_) => Some(decode_participant(
            receiver_colleague,
            receiver_kind,
            receiver_user,
            receiver_agent,
        )?),
        None => None,
    };
    Ok(FeedMessage {
        seq,
        kind,
        sender,
        owner_agent_id,
        receiver,
        body,
        request_id,
        created_at,
    })
}

/// Parse a colleague LEFT JOIN's satellite columns into the typed
/// [`Participant`] via the one canonical decode (`types::participant`). A
/// malformed shape is a schema/code disagreement, surfaced as `Backend`.
fn decode_participant(
    colleague: Option<ColleagueId>,
    kind: Option<ColleagueKind>,
    user_id: Option<UserId>,
    agent_id: Option<AgentId>,
) -> Result<Participant, ThreadError> {
    Participant::try_from((colleague, kind, user_id, agent_id))
        .map_err(|e| ThreadError::Backend(format!("decode feed participant: {e}")))
}

/// Map one feed row to `viewer`'s perspective.
///
/// - Non-`Posted` rows are `viewer`'s own private artifacts (the SQL filter
///   guarantees `owner == viewer`'s agent); the stored body is already in the
///   owner's perspective (Assistant for reasoning/tool_use, User for
///   tool_result/system_note), so it passes through unchanged.
/// - `Posted` rows authored by `viewer` pass through (Assistant).
/// - `Posted` rows authored by anyone else become `User` content.
///
/// TODO(P8/P10): prepend an author label (`[name]: …`) on others' posted rows
/// once the participant roster / privileged name read is wired.
fn map_row_for_viewer(
    kind: MessageKind,
    sender: Option<ColleagueId>,
    stored: ChatMessage,
    viewer: ColleagueId,
) -> ChatMessage {
    if kind != MessageKind::Posted {
        return stored;
    }
    if sender == Some(viewer) {
        return stored;
    }
    match stored {
        ChatMessage::User(blocks) => ChatMessage::User(blocks),
        ChatMessage::Assistant(blocks) => ChatMessage::User(assistant_to_user_blocks(blocks)),
    }
}

/// Fold another agent's posted Assistant blocks into User content. Posted
/// messages are text; reasoning/tool-call blocks (if any leak in) are dropped —
/// a peer's private thinking is never ingested.
fn assistant_to_user_blocks(blocks: Vec<AssistantContent>) -> Vec<UserContent> {
    blocks
        .into_iter()
        .filter_map(|b| match b {
            AssistantContent::Text(t) => Some(UserContent::Text(t)),
            AssistantContent::Reasoning(_) | AssistantContent::ToolCall(_) => None,
        })
        .collect()
}
