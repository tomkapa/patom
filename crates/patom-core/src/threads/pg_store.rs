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

use std::collections::{HashMap, HashSet};

use crate::agents::AgentId;
use crate::auth::{Caller, UserId, run_as_user, run_privileged};
use crate::channels::{ChannelId, ChannelName};
use crate::clock::SharedClock;
use crate::colleagues::{ColleagueId, ColleagueKind, ColleagueName};
use crate::provider::{AssistantContent, ChatMessage, ToolCallId, UserContent};
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::ThreadError;
use super::limits::{
    MAX_CHANNELS_FOR_COLLEAGUE, MAX_CONTEXT_MESSAGES, MAX_READ_CHANNEL_MESSAGES, MAX_THREAD_FEED,
    MAX_THREAD_LIST, MAX_TOOL_RESULT_CHARS, READ_CHANNEL_BODY_MAX_CHARS, ROOT_SNIPPET_MAX_CHARS,
};
use super::traits::{
    AgentThreadId, ChannelFeedRow, ChannelRef, ContextTail, FeedMessage, MessageKind, NewMessage,
    RootSummary, Seq, TailRow, ThreadCompaction, ThreadId, ThreadListItem, ThreadMessageId,
    ThreadParticipants, ThreadScope, ThreadStore,
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
/// artifacts, with `seq > since`. `$1` thread, `$2` agent, `$3` since-seq, `$4`
/// row limit (the windowing floor, #182). The inner query takes the most-recent
/// `$4` rows (`seq DESC LIMIT`); the outer flips them back to chronological
/// `seq ASC` so the prompt reads old→new. Resolves the sender's canonical
/// display name (agent name / user display name / email local-part) so the feed
/// can attribute speakers in a multi-party thread; the platform-label override
/// is applied in Rust. Keep the COALESCE in sync with
/// `colleagues::pg_store::display_name_expr!` (same rule, different aliases).
const CONTEXT_SQL: &str = "SELECT sub.seq, sub.kind, sub.sender_colleague_id, \
            sub.sender_name, sub.body \
     FROM ( \
       SELECT m.seq, m.kind, m.sender_colleague_id, \
              COALESCE(sa.name, su.display_name, split_part(su.email, '@', 1)) AS sender_name, \
              m.body \
       FROM thread_messages m \
       LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
       LEFT JOIN agents sa ON sa.id = sc.agent_id \
       LEFT JOIN users  su ON su.id = sc.user_id \
       WHERE m.thread_id = $1 AND (m.kind = 'posted' OR m.owner_agent_id = $2) \
         AND m.seq > $3 \
       ORDER BY m.seq DESC \
       LIMIT $4 \
     ) sub \
     ORDER BY sub.seq ASC";

/// The DM-visibility predicate: a channel-less thread is visible to its
/// creator OR its counterpart. This is a **tenant-isolation** rule, repeated
/// across `FEED_SQL` / `visible_to` / `is_channel_member` / `LIST_WHERE_DM`
/// (each binds the user at a different `$N`). One source of truth so a future
/// change (e.g. a third DM participant) can't silently diverge between the
/// read paths. `concat!` over string literals only — §10-safe (no caller
/// input ever reaches the SQL text).
macro_rules! dm_visible_to {
    ($u:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM colleagues cb \
                     WHERE cb.id = t.created_by_colleague_id AND cb.user_id = ",
            $u,
            ") OR EXISTS (SELECT 1 FROM colleagues cp \
                          WHERE cp.id = t.dm_counterpart_colleague_id AND cp.user_id = ",
            $u,
            ")"
        )
    };
}

/// G2 flat-feed read. Both participant sides joined to `colleagues` for their
/// satellite columns (kind / user_id / agent_id) so the HTTP boundary decodes a
/// `Participant`/`MessageSender` and enriches a human name/avatar. The
/// visibility gate mirrors `list_threads` (channel membership, or DM ownership)
/// and pins the active org (`$2`). Pages backward on the `seq` keyset
/// (`$4`); ordered DESC for the LIMIT then reversed to ascending by the caller.
const FEED_SQL: &str = concat!(
    "SELECT m.seq, m.kind, \
        m.sender_colleague_id, sc.kind, sc.user_id, sc.agent_id, \
        m.owner_agent_id, \
        m.receiver_colleague_id, rc.kind, rc.user_id, rc.agent_id, \
        m.body, m.request_id, m.idempotency_key, m.created_at \
     FROM thread_messages m \
     JOIN threads t ON t.id = m.thread_id \
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
     LEFT JOIN colleagues rc ON rc.id = m.receiver_colleague_id \
     WHERE m.thread_id = $1 AND t.org_id = $2 \
       AND (CASE WHEN t.channel_id IS NULL THEN ",
    dm_visible_to!("$3"),
    "       ELSE \
                EXISTS (SELECT 1 FROM channel_members cm \
                        WHERE cm.channel_id = t.channel_id AND cm.user_id = $3) \
                AND EXISTS (SELECT 1 FROM channels c \
                            WHERE c.id = t.channel_id AND c.archived_at IS NULL) \
            END) \
       AND ($4::bigint IS NULL OR m.seq < $4) \
     ORDER BY m.seq DESC \
     LIMIT $5"
);

/// Channel-level history read — the `read_channel` digest source (#199).
/// `posted` rows from **every** thread bound to `$1` (the ambient channel
/// thread plus every @mention sub-thread share `threads.channel_id`), newest
/// first for the `$3` LIMIT, with `$2` an optional `created_at` floor. The body
/// is preview-capped in SQL (`LEFT`, $-bound to `$4`) so an unbounded `TEXT`
/// never crosses the wire (§5/§10). The sender's canonical display name is
/// resolved exactly as `CONTEXT_SQL` (agent name / user display name / email
/// local-part); a System row (NULL sender) yields a NULL name. The caller flips
/// the DESC page back to chronological order.
const CHANNEL_FEED_SQL: &str = "SELECT m.created_at, \
            COALESCE(sa.name, su.display_name, split_part(su.email, '@', 1)) AS sender_name, \
            LEFT(m.body->'contents'->0->>'value', $4) AS preview \
     FROM thread_messages m \
     JOIN threads t ON t.id = m.thread_id \
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
     LEFT JOIN agents sa ON sa.id = sc.agent_id \
     LEFT JOIN users  su ON su.id = sc.user_id \
     WHERE t.channel_id = $1 AND m.kind = 'posted' \
       AND ($2::timestamptz IS NULL OR m.created_at >= $2) \
     ORDER BY m.created_at DESC \
     LIMIT $3";

/// `list_threads` page with the timeline enrichment: the root posted row's
/// snippet + sender satellites (LATERAL, first `posted` by `seq`) and the
/// posted-row count. The snippet is capped in SQL (`LEFT`, $-bound to
/// [`ROOT_SNIPPET_MAX_CHARS`]) so an oversized body never crosses the wire.
/// `{where}` is one of the two scope predicates below — assembled by
/// `format!` from `const` fragments only, never from caller input (§10).
macro_rules! list_sql {
    ($where:expr) => {
        format!(
            "SELECT t.id, t.channel_id, t.last_activity_at, \
                    r.snippet, r.sc_id, r.sc_kind, r.sc_user, r.sc_agent, r.created_at, \
                    COALESCE(pc.posted_count, 0) \
             FROM threads t \
             LEFT JOIN LATERAL ( \
                 SELECT LEFT(m.body->'contents'->0->>'value', $4) AS snippet, \
                        m.sender_colleague_id AS sc_id, sc.kind AS sc_kind, \
                        sc.user_id AS sc_user, sc.agent_id AS sc_agent, \
                        m.created_at \
                 FROM thread_messages m \
                 LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
                 WHERE m.thread_id = t.id AND m.kind = 'posted' \
                 ORDER BY m.seq ASC LIMIT 1 \
             ) r ON TRUE \
             LEFT JOIN LATERAL ( \
                 SELECT COUNT(*) AS posted_count FROM thread_messages m \
                 WHERE m.thread_id = t.id AND m.kind = 'posted' \
             ) pc ON TRUE \
             WHERE t.org_id = $1 AND {} \
             ORDER BY t.last_activity_at DESC LIMIT $3",
            $where
        )
    };
}

/// Channel scope: member-gated + not archived. `$2` = channel, `$5` unused.
const LIST_WHERE_CHANNEL: &str = "t.channel_id = $2 \
       AND EXISTS (SELECT 1 FROM channel_members cm \
                   WHERE cm.channel_id = t.channel_id AND cm.user_id = $5) \
       AND EXISTS (SELECT 1 FROM channels c \
                   WHERE c.id = t.channel_id AND c.archived_at IS NULL)";

/// DM scope: the caller is the creator or the counterpart; `$2` optionally
/// narrows to the pair with one colleague (either orientation).
const LIST_WHERE_DM: &str = concat!(
    "t.channel_id IS NULL AND (",
    dm_visible_to!("$5"),
    ") AND ($2::uuid IS NULL \
            OR t.dm_counterpart_colleague_id = $2 \
            OR t.created_by_colleague_id = $2)"
);

#[async_trait]
impl ThreadStore for PgThreadStore {
    #[tracing::instrument(skip_all, name = "thread.create", fields(patom.thread.id = tracing::field::Empty))]
    async fn create_thread(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
        root_message_id: Option<ThreadMessageId>,
        created_by: ColleagueId,
        dm_counterpart: Option<ColleagueId>,
    ) -> Result<ThreadId, ThreadError> {
        // The DB CHECK forbids both-set; the missing half (a DM must name its
        // counterpart) is this code-side invariant — see migration 66.
        assert!(
            channel_id.is_none() || dm_counterpart.is_none(),
            "invariant: a channel thread carries no DM counterpart"
        );
        assert!(
            channel_id.is_some() || dm_counterpart.is_some(),
            "invariant: a DM thread names its counterpart"
        );
        let now = self.now();
        let id = ThreadId::new();
        run_as_user(&self.pool, caller.user_id, async |tx| {
            sqlx::query(
                "INSERT INTO threads \
                   (id, org_id, channel_id, root_message_id, created_by_colleague_id, \
                    dm_counterpart_colleague_id, created_at, last_activity_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
            )
            .bind(id)
            .bind(caller.org_id)
            .bind(channel_id)
            .bind(root_message_id)
            .bind(created_by)
            .bind(dm_counterpart)
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
        let idem = message
            .idempotency_key
            .as_ref()
            .map(crate::runtime::IdempotencyKey::as_str);
        // One round trip: bump the per-thread seq, insert the row at that seq,
        // and bump last_activity_at — all gated on the thread existing (`t`).
        // A duplicate `idempotency_key` (concurrent retry of the same submit)
        // hits `ON CONFLICT DO NOTHING`; the fallback SELECT below returns the
        // winner's row so retries converge on one message. The bumped-but-
        // unused seq leaves a hole, which the ordering tolerates.
        let row: Option<(i64,)> =
            run_as_user::<Option<(i64,)>, ThreadError>(&self.pool, caller.user_id, async |tx| {
                let inserted: Option<(i64,)> = sqlx::query_as(
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
                          receiver_colleague_id, body, request_id, org_id, created_at, \
                          idempotency_key) \
                     SELECT $2, $1, seqx.next_seq, $3, $4, $5, $6, $7, $8, t.org_id, $9, $10 \
                     FROM seqx, t \
                     ON CONFLICT (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL \
                         DO NOTHING \
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
                .bind(&body)
                .bind(message.request_id)
                .bind(now)
                .bind(idem)
                .fetch_optional(&mut **tx)
                .await?;
                Ok(inserted)
            })
            .await?;
        if let Some((_seq,)) = row {
            // The `seq` row confirms the insert fired (thread exists); return
            // the surface id we minted, which callers thread into
            // reply-roots / triggers.
            return Ok(id);
        }
        // No insert: either the thread is missing, or an idempotent retry lost
        // the (org, idempotency_key) race — return the winner's row id.
        if let Some(key) = idem {
            let existing: Option<(ThreadMessageId,)> =
                run_as_user::<Option<(ThreadMessageId,)>, ThreadError>(
                    &self.pool,
                    caller.user_id,
                    async |tx| {
                        Ok(sqlx::query_as(
                        "SELECT id FROM thread_messages WHERE org_id = $1 AND idempotency_key = $2",
                    )
                    .bind(caller.org_id)
                    .bind(key)
                    .fetch_optional(&mut **tx)
                    .await?)
                    },
                )
                .await?;
            if let Some((existing_id,)) = existing {
                return Ok(existing_id);
            }
        }
        Err(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.list_threads", fields(patom.org.id = %caller.org_id, patom.thread.scope = ?scope, patom.thread.count = tracing::field::Empty))]
    async fn list_threads(
        &self,
        caller: &Caller,
        scope: ThreadScope,
    ) -> Result<Vec<ThreadListItem>, ThreadError> {
        let org = caller.org_id;
        let user = caller.user_id;
        // Channel view: gated on the caller's membership + channel not archived
        // (visible to every member, not just the creator — P7). DM view: the
        // caller's channel-less threads — created by them OR addressed to them
        // (the counterpart), optionally narrowed to one pair. Org-pinned so a
        // multi-org member's other workspaces never leak in (RLS gates
        // membership, not active org).
        let (sql, scope_id) = match scope {
            ThreadScope::Channel(channel) => {
                (list_sql!(LIST_WHERE_CHANNEL), Some(channel.as_uuid()))
            }
            ThreadScope::Dms { counterpart } => (
                list_sql!(LIST_WHERE_DM),
                counterpart.map(ColleagueId::as_uuid),
            ),
        };
        let rows: Vec<ListRow> =
            run_as_user::<Vec<ListRow>, ThreadError>(&self.pool, user, async |tx| {
                Ok(sqlx::query_as(&sql)
                    .bind(org)
                    .bind(scope_id)
                    .bind(MAX_THREAD_LIST)
                    .bind(ROOT_SNIPPET_MAX_CHARS)
                    .bind(user)
                    .fetch_all(&mut **tx)
                    .await?)
            })
            .await?;
        tracing::Span::current().record("patom.thread.count", rows.len());
        rows.into_iter().map(list_row_to_item).collect()
    }

    #[tracing::instrument(skip_all, name = "thread.is_channel_member", fields(patom.thread.id = %thread, patom.user.id = %user_id))]
    async fn is_channel_member(
        &self,
        thread: ThreadId,
        user_id: crate::auth::UserId,
    ) -> Result<bool, ThreadError> {
        // One round trip: resolve the thread's channel and, for a channel
        // thread, test membership; a DM thread (NULL channel) is reachable
        // only by its pair — creator or counterpart. Privileged — the agent is
        // org-global and the (channel, user) pair is fully qualified, so no
        // cross-org leak.
        let row: Option<(bool,)> =
            run_privileged::<Option<(bool,)>, ThreadError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(concat!(
                    "SELECT CASE WHEN t.channel_id IS NULL THEN ",
                    dm_visible_to!("$2"),
                    " ELSE EXISTS ( \
                             SELECT 1 FROM channel_members m \
                             WHERE m.channel_id = t.channel_id AND m.user_id = $2 \
                         ) END \
                     FROM threads t WHERE t.id = $1"
                ))
                .bind(thread)
                .bind(user_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        row.map(|(ok,)| ok).ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.colleague_in_channel", fields(patom.channel.id = %channel, patom.colleague.id = %colleague))]
    async fn colleague_in_channel(
        &self,
        channel: ChannelId,
        colleague: ColleagueId,
    ) -> Result<bool, ThreadError> {
        // Union of human (channel_members, joined through the colleague's user)
        // and agent (channel_agent_members) membership. Privileged — the pair is
        // fully qualified and the channel's org bounds it.
        let row: (bool,) = run_privileged::<(bool,), ThreadError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT EXISTS ( \
                       SELECT 1 FROM channel_agent_members am \
                       WHERE am.channel_id = $1 AND am.colleague_id = $2 \
                   ) OR EXISTS ( \
                       SELECT 1 FROM channel_members m \
                       JOIN colleagues c ON c.user_id = m.user_id AND c.org_id = m.org_id \
                       WHERE m.channel_id = $1 AND c.id = $2 \
                   )",
            )
            .bind(channel)
            .bind(colleague)
            .fetch_one(&mut **tx)
            .await?)
        })
        .await?;
        Ok(row.0)
    }

    #[tracing::instrument(skip_all, name = "thread.channels_for_colleague", fields(patom.colleague.id = %colleague, patom.channel.count = tracing::field::Empty))]
    async fn channels_for_colleague(
        &self,
        org: crate::auth::OrgId,
        colleague: ColleagueId,
    ) -> Result<Vec<ChannelRef>, ThreadError> {
        let rows: Vec<(ChannelId, String)> =
            run_privileged::<Vec<(ChannelId, String)>, ThreadError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT ch.id, ch.name FROM channels ch \
                     WHERE ch.org_id = $1 AND ch.archived_at IS NULL \
                       AND ( \
                           EXISTS ( \
                               SELECT 1 FROM channel_agent_members am \
                               WHERE am.channel_id = ch.id AND am.colleague_id = $2 \
                           ) OR EXISTS ( \
                               SELECT 1 FROM channel_members m \
                               JOIN colleagues c ON c.user_id = m.user_id AND c.org_id = m.org_id \
                               WHERE m.channel_id = ch.id AND c.id = $2 \
                           ) \
                       ) \
                     ORDER BY ch.name \
                     LIMIT $3",
                )
                .bind(org)
                .bind(colleague)
                .bind(MAX_CHANNELS_FOR_COLLEAGUE)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        tracing::Span::current().record("patom.channel.count", rows.len());
        rows.into_iter()
            .map(|(id, name)| {
                // The name was CHECK-validated on write, so a parse failure here
                // is a corrupt row, not expected input — surface it as Backend.
                let name = ChannelName::try_from(name.as_str())
                    .map_err(|e| ThreadError::Backend(format!("invalid channel name: {e}")))?;
                Ok(ChannelRef { id, name })
            })
            .collect()
    }

    #[tracing::instrument(skip_all, name = "thread.add_agent_to_channel", fields(patom.channel.id = %channel, patom.colleague.id = %colleague))]
    async fn add_agent_to_channel(
        &self,
        org: crate::auth::OrgId,
        channel: ChannelId,
        colleague: ColleagueId,
    ) -> Result<(), ThreadError> {
        let now = self.now();
        run_privileged::<(), ThreadError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO channel_agent_members (channel_id, colleague_id, org_id, added_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (channel_id, colleague_id) DO NOTHING",
            )
            .bind(channel)
            .bind(colleague)
            .bind(org)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
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

    #[tracing::instrument(skip_all, name = "thread.channel_feed", fields(patom.channel.id = %channel, patom.feed.count = tracing::field::Empty))]
    async fn channel_feed(
        &self,
        channel: ChannelId,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<ChannelFeedRow>, ThreadError> {
        // One channel-feed row: (created_at, resolved sender name, body preview).
        type Row = (DateTime<Utc>, Option<String>, Option<String>);
        // §5: clamp the caller's limit into the bounded window so a stray value
        // can't widen the scan; the post-query assert proves the LIMIT held.
        let cap = limit.clamp(1, MAX_READ_CHANNEL_MESSAGES);
        // Privileged: the channel id is fully qualified (globally unique) and the
        // `read_channel` tool gates membership before calling, so no caller
        // principal is threaded here.
        let mut rows: Vec<Row> = run_privileged::<Vec<Row>, ThreadError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(CHANNEL_FEED_SQL)
                .bind(channel)
                .bind(since)
                .bind(cap)
                .bind(READ_CHANNEL_BODY_MAX_CHARS)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;
        // The SQL LIMIT bounds the batch; assert it held so a query change that
        // drops the LIMIT trips here rather than shipping an unbounded page (§6).
        assert!(
            i64::try_from(rows.len()).unwrap_or(i64::MAX) <= cap,
            "invariant: channel_feed respects its LIMIT"
        );
        // Query is DESC for the keyset LIMIT; a digest reads oldest→newest.
        rows.reverse();
        tracing::Span::current().record("patom.feed.count", rows.len());
        Ok(rows
            .into_iter()
            .map(|(created_at, author, preview)| ChannelFeedRow {
                created_at,
                author,
                body_preview: preview.unwrap_or_default(),
            })
            .collect())
    }

    #[tracing::instrument(skip_all, name = "thread.dm_counterpart", fields(patom.thread.id = %thread))]
    async fn dm_counterpart(&self, thread: ThreadId) -> Result<Option<ColleagueId>, ThreadError> {
        // Privileged point lookup — `POST /prompts` resolves the implicit DM
        // receiver before any tenant write. `None` covers both a channel
        // thread and a legacy/degraded DM.
        let row: Option<(Option<ColleagueId>,)> =
            run_privileged::<Option<(Option<ColleagueId>,)>, ThreadError>(&self.pool, async |tx| {
                Ok(
                    sqlx::query_as("SELECT dm_counterpart_colleague_id FROM threads WHERE id = $1")
                        .bind(thread)
                        .fetch_optional(&mut **tx)
                        .await?,
                )
            })
            .await?;
        row.map(|(counterpart,)| counterpart)
            .ok_or(ThreadError::NotFound(thread))
    }

    #[tracing::instrument(skip_all, name = "thread.participants", fields(patom.thread.id = %thread))]
    async fn thread_participants(
        &self,
        thread: ThreadId,
    ) -> Result<ThreadParticipants, ThreadError> {
        // Privileged read — the agent worker is org-global within its org. The
        // creator is a point lookup; senders are the distinct `posted` authors
        // in first-seen order, capped (§5).
        let cap = i64::try_from(crate::colleagues::MAX_PARTICIPANTS_INLINE).unwrap_or(i64::MAX);
        run_privileged::<ThreadParticipants, ThreadError>(&self.pool, async |tx| {
            let creator: Option<(Option<ColleagueId>,)> =
                sqlx::query_as("SELECT created_by_colleague_id FROM threads WHERE id = $1")
                    .bind(thread)
                    .fetch_optional(&mut **tx)
                    .await?;
            let creator = creator.and_then(|(c,)| c);

            let senders: Vec<(ColleagueId,)> = sqlx::query_as(
                "SELECT sender_colleague_id FROM thread_messages \
                  WHERE thread_id = $1 AND kind = 'posted' AND sender_colleague_id IS NOT NULL \
                  GROUP BY sender_colleague_id \
                  ORDER BY MIN(seq) ASC \
                  LIMIT $2",
            )
            .bind(thread)
            .bind(cap)
            .fetch_all(&mut **tx)
            .await?;
            let senders = senders.into_iter().map(|(s,)| s).collect();

            Ok(ThreadParticipants { creator, senders })
        })
        .await
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

    #[tracing::instrument(skip_all, name = "thread.advance_reflection_checkpoint", fields(patom.thread.id = %thread, patom.agent.id = %agent))]
    async fn advance_reflection_checkpoint(
        &self,
        org_id: crate::auth::OrgId,
        agent: AgentId,
        thread: ThreadId,
        up_to_message_id: ThreadMessageId,
    ) -> Result<(), ThreadError> {
        let now = self.now();
        // Privileged upsert — worker-side cognition has no per-request principal.
        // The `(agent_id, thread_id)` PK makes this idempotent; advancing
        // `last_message_id` is what stops the scheduler re-enqueuing each tick.
        run_privileged::<(), ThreadError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO reflection_checkpoints \
                   (agent_id, thread_id, last_message_id, org_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (agent_id, thread_id) \
                 DO UPDATE SET last_message_id = EXCLUDED.last_message_id",
            )
            .bind(agent)
            .bind(thread)
            .bind(up_to_message_id)
            .bind(org_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
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
                Ok(sqlx::query_as(concat!(
                    "SELECT EXISTS( \
                         SELECT 1 FROM threads t \
                         WHERE t.id = $1 AND t.org_id = $2 \
                           AND (CASE WHEN t.channel_id IS NULL THEN ",
                    dm_visible_to!("$3"),
                    "       ELSE \
                                    EXISTS (SELECT 1 FROM channel_members cm \
                                            WHERE cm.channel_id = t.channel_id AND cm.user_id = $3) \
                                    AND EXISTS (SELECT 1 FROM channels c \
                                                WHERE c.id = t.channel_id AND c.archived_at IS NULL) \
                                END))"
                ))
                .bind(thread)
                .bind(org)
                .bind(user)
                .fetch_one(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.0)
    }

    #[tracing::instrument(skip_all, name = "thread.context_tail", fields(patom.thread.id = %thread, patom.agent.id = %agent, patom.depth = since.get(), patom.history.count = tracing::field::Empty))]
    async fn context_tail(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
        since: Seq,
        overrides: &HashMap<ColleagueId, ColleagueName>,
    ) -> Result<ContextTail, ThreadError> {
        type Row = (
            i64,
            MessageKind,
            Option<ColleagueId>,
            Option<String>,
            serde_json::Value,
        );
        let rows: Vec<Row> = run_privileged::<Vec<Row>, ThreadError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(CONTEXT_SQL)
                .bind(thread)
                .bind(agent)
                .bind(since.get())
                .bind(MAX_CONTEXT_MESSAGES)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;

        let mut mapped: Vec<(Seq, MessageKind, ChatMessage)> = Vec::with_capacity(rows.len());
        for (raw_seq, kind, sender, sender_name, body) in rows {
            let seq = Seq::try_from(raw_seq)
                .map_err(|e| ThreadError::Backend(format!("decode feed seq: {e}")))?;
            let stored: ChatMessage = serde_json::from_value(body)
                .map_err(|e| ThreadError::Backend(format!("deserialize message: {e}")))?;
            // Platform label wins over the canonical sender name; both are
            // display-only — addressing/identity stays on the colleague id.
            let label = sender
                .and_then(|s| overrides.get(&s))
                .map(|n| n.as_str().to_owned())
                .or(sender_name);
            let mut message = map_row_for_viewer(kind, sender, label, stored, viewer);
            // Prompt-only safety net: a fat tool_result is render-capped here;
            // the immutable feed row is untouched (a re-read returns the bytes).
            cap_tool_results_in_place(&mut message, seq);
            mapped.push((seq, kind, message));
        }

        // Re-pair tool_use/tool_result, then drop any leading `tool_result`
        // whose `tool_use` fell outside the window (the LIMIT can cut a pair's
        // head) — the provider rejects an orphaned result.
        let repaired = drop_orphan_tool_results(repair_tool_pairs(mapped));
        let rows: Vec<TailRow> = repaired
            .into_iter()
            .map(|(seq, message)| TailRow { seq, message })
            .collect();

        // The windowing floor: the SQL LIMIT bounds the read, trims only shrink
        // it, so the prompt is bounded regardless of summary state (CLAUDE.md §6,
        // assert both directions of the invariant we rely on downstream).
        let cap = usize::try_from(MAX_CONTEXT_MESSAGES).unwrap_or(usize::MAX);
        assert!(
            rows.len() <= cap,
            "context_tail exceeded the windowing floor"
        );
        tracing::Span::current().record("patom.history.count", rows.len());
        Ok(ContextTail { rows })
    }

    #[tracing::instrument(skip_all, name = "thread.load_compaction", fields(patom.thread.id = %thread, patom.agent.id = %agent))]
    async fn load_compaction(
        &self,
        thread: ThreadId,
        agent: AgentId,
    ) -> Result<Option<ThreadCompaction>, ThreadError> {
        type Row = (String, i64, i32, i32, Option<DateTime<Utc>>);
        let row: Option<Row> = run_privileged::<Option<Row>, ThreadError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT summary, covers_through_seq, summary_tokens, failed_attempts, \
                        cooldown_until \
                 FROM thread_compactions WHERE thread_id = $1 AND agent_id = $2",
            )
            .bind(thread)
            .bind(agent)
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let Some((summary, covers, summary_tokens, failed_attempts, cooldown_until)) = row else {
            return Ok(None);
        };
        let covers_through_seq = Seq::try_from(covers)
            .map_err(|e| ThreadError::Backend(format!("decode covers_through_seq: {e}")))?;
        Ok(Some(ThreadCompaction {
            summary,
            covers_through_seq,
            summary_tokens,
            failed_attempts,
            cooldown_until,
        }))
    }

    #[tracing::instrument(skip_all, name = "thread.save_compaction", fields(patom.thread.id = %thread, patom.agent.id = %agent, patom.org.id = %org))]
    async fn save_compaction(
        &self,
        org: crate::auth::OrgId,
        thread: ThreadId,
        agent: AgentId,
        summary: &str,
        covers_through_seq: Seq,
        summary_tokens: i32,
    ) -> Result<(), ThreadError> {
        run_privileged::<(), ThreadError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO thread_compactions \
                     (thread_id, agent_id, org_id, summary, covers_through_seq, summary_tokens, \
                      failed_attempts, cooldown_until, version, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0, NULL, 1, now()) \
                 ON CONFLICT (thread_id, agent_id) DO UPDATE SET \
                     summary = EXCLUDED.summary, \
                     covers_through_seq = EXCLUDED.covers_through_seq, \
                     summary_tokens = EXCLUDED.summary_tokens, \
                     failed_attempts = 0, \
                     cooldown_until = NULL, \
                     version = thread_compactions.version + 1, \
                     updated_at = now()",
            )
            .bind(thread)
            .bind(agent)
            .bind(org)
            .bind(summary)
            .bind(covers_through_seq.get())
            .bind(summary_tokens)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    #[tracing::instrument(skip_all, name = "thread.bump_cooldown", fields(patom.thread.id = %thread, patom.agent.id = %agent, patom.org.id = %org))]
    async fn bump_cooldown(
        &self,
        org: crate::auth::OrgId,
        thread: ThreadId,
        agent: AgentId,
        cooldown_until: DateTime<Utc>,
    ) -> Result<(), ThreadError> {
        run_privileged::<(), ThreadError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO thread_compactions \
                     (thread_id, agent_id, org_id, summary, covers_through_seq, summary_tokens, \
                      failed_attempts, cooldown_until, version, updated_at) \
                 VALUES ($1, $2, $3, '', 0, 0, 1, $4, 1, now()) \
                 ON CONFLICT (thread_id, agent_id) DO UPDATE SET \
                     failed_attempts = thread_compactions.failed_attempts + 1, \
                     cooldown_until = $4, \
                     updated_at = now()",
            )
            .bind(thread)
            .bind(agent)
            .bind(org)
            .bind(cooldown_until)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}

/// Render-cap an oversized `tool_result` body **in place** for the prompt only.
/// The full row stays in `thread_messages`; this rewrites the in-memory copy as
/// `head + [… omitted N chars · thread seq S …] + tail` so the body stays
/// recoverable (operator can fetch the row by thread + seq) but can't dominate
/// the context window. No-op for bodies within `MAX_TOOL_RESULT_CHARS`.
fn cap_tool_results_in_place(message: &mut ChatMessage, seq: Seq) {
    let ChatMessage::User(contents) = message else {
        return;
    };
    for content in contents.iter_mut() {
        let UserContent::ToolResult(result) = content else {
            continue;
        };
        if let Some(capped) = cap_tool_result_output(&result.output, seq) {
            result.output = capped;
        }
    }
}

/// Build the capped rendering of an over-limit body, or `None` if it fits.
/// Char-boundary safe (counts and slices by `char`, never bytes).
fn cap_tool_result_output(output: &str, seq: Seq) -> Option<String> {
    let total = output.chars().count();
    if total <= MAX_TOOL_RESULT_CHARS {
        return None;
    }
    let keep = MAX_TOOL_RESULT_CHARS / 2;
    let head: String = output.chars().take(keep).collect();
    let tail: String = output.chars().skip(total - keep).collect();
    let omitted = total - keep - keep;
    Some(format!(
        "{head}\n[… omitted {omitted} chars · thread seq {} …]\n{tail}",
        seq.get()
    ))
}

/// Drop rows whose `tool_result` answers a `tool_use` that isn't in this window.
/// After [`repair_tool_pairs`] a result follows its use, so a result whose call
/// id appears nowhere among the window's `tool_use` rows is an orphan the LIMIT
/// cut off; the provider would reject it.
fn drop_orphan_tool_results(mut rows: Vec<(Seq, ChatMessage)>) -> Vec<(Seq, ChatMessage)> {
    let mut call_ids: HashSet<ToolCallId> = HashSet::new();
    for (_, message) in &rows {
        if let ChatMessage::Assistant(contents) = message {
            for content in contents {
                if let AssistantContent::ToolCall(call) = content {
                    call_ids.insert(call.id.clone());
                }
            }
        }
    }
    rows.retain(|(_, message)| match message {
        ChatMessage::User(contents) => match contents.first() {
            Some(UserContent::ToolResult(result)) => call_ids.contains(&result.call_id),
            _ => true,
        },
        ChatMessage::Assistant(_) => true,
    });
    rows
}

/// Re-pair an agent's `tool_use` with its `tool_result` (note 13).
///
/// A turn appends the assistant's `tool_use` row and then the matching
/// `tool_result` row at consecutive `seq`s — but threads are multi-writer, so a
/// peer's `posted` row can land *between* them by `seq` while the tool runs.
/// The provider requires the `tool_result` user message to immediately follow
/// the `tool_use` assistant message, so this stable pass defers any rows that
/// fall between a `ToolUse` and its `ToolResult` to *after* the pair, leaving
/// every other relative order intact. Only `posted`/`reasoning`/`system_note`
/// rows ever interleave (never another `ToolUse`, since a turn closes its
/// tool_use before the next), so the single-pending-result model is sufficient.
fn repair_tool_pairs(rows: Vec<(Seq, MessageKind, ChatMessage)>) -> Vec<(Seq, ChatMessage)> {
    let mut out: Vec<(Seq, ChatMessage)> = Vec::with_capacity(rows.len());
    let mut deferred: Vec<(Seq, ChatMessage)> = Vec::new();
    let mut awaiting_result = false;
    for (seq, kind, msg) in rows {
        if awaiting_result {
            if kind == MessageKind::ToolResult {
                out.push((seq, msg)); // the result, immediately after its tool_use
                awaiting_result = false;
                out.append(&mut deferred); // flush rows that interleaved the pair
            } else {
                deferred.push((seq, msg)); // hold a peer post until the pair closes
            }
        } else {
            let is_tool_use = kind == MessageKind::ToolUse;
            out.push((seq, msg));
            awaiting_result = is_tool_use;
        }
    }
    // An unclosed tool_use (turn cancelled before its result) — flush whatever
    // we held; the provider would reject the orphan regardless, but we never
    // drop rows.
    out.append(&mut deferred);
    out
}

/// One [`FEED_SQL`] row: seq, kind, the sender's colleague satellites, the
/// owner agent, the receiver's colleague satellites, body, request id, the
/// client idempotency key, ts.
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
    Option<String>,
    DateTime<Utc>,
);

/// One `list_sql!` row: thread identity + the root posted row's snippet and
/// sender satellites (all NULL when the thread has no posted row yet) + the
/// posted-row count.
#[allow(clippy::type_complexity)]
type ListRow = (
    ThreadId,
    Option<ChannelId>,
    DateTime<Utc>,
    Option<String>,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    Option<DateTime<Utc>>,
    i64,
);

/// Decode one [`ListRow`] into the public [`ThreadListItem`]. The root exists
/// iff the LATERAL matched a posted row (detected on its `created_at`); a
/// posted row whose body has no leading text block degrades to an empty
/// snippet rather than an error.
fn list_row_to_item(row: ListRow) -> Result<ThreadListItem, ThreadError> {
    let (
        thread_id,
        channel_id,
        last_activity_at,
        snippet,
        sender_colleague,
        sender_kind,
        sender_user,
        sender_agent,
        root_created_at,
        posted_count,
    ) = row;
    let root = match root_created_at {
        Some(created_at) => Some(RootSummary {
            snippet: snippet.unwrap_or_default(),
            sender: MessageSender::from(decode_participant(
                sender_colleague,
                sender_kind,
                sender_user,
                sender_agent,
            )?),
            created_at,
        }),
        None => None,
    };
    Ok(ThreadListItem {
        thread_id,
        channel_id,
        last_activity_at,
        root,
        reply_count: (posted_count - 1).max(0),
    })
}

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
        client_key,
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
        client_key,
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
/// - `Posted` rows are stored in a neutral `User` perspective by every writer
///   (`send_message`, the HTTP prompt route, the Slack bridge). This function
///   re-tags them to the viewer's perspective: a post authored by `viewer`
///   becomes `Assistant` (the viewer's own speech); a post by anyone else stays
///   `User`, attributed to its sender. Re-tagging the viewer's own posts is
///   load-bearing — without it the agent re-reads its own send_message output
///   as a `User` turn mid-turn and starts replying to itself.
fn map_row_for_viewer(
    kind: MessageKind,
    sender: Option<ColleagueId>,
    sender_label: Option<String>,
    stored: ChatMessage,
    viewer: ColleagueId,
) -> ChatMessage {
    if kind != MessageKind::Posted {
        return stored;
    }
    if sender == Some(viewer) {
        // The viewer authored this post — render it as their own speech
        // regardless of the neutral storage perspective.
        return match stored {
            ChatMessage::Assistant(blocks) => ChatMessage::Assistant(blocks),
            ChatMessage::User(blocks) => ChatMessage::Assistant(user_to_assistant_blocks(blocks)),
        };
    }
    // A peer's post becomes a User message attributed to its sender, so the
    // agent can tell speakers apart in a multi-party thread. The agent's own
    // posts (filtered above) are never prefixed.
    let blocks = match stored {
        ChatMessage::User(blocks) => blocks,
        ChatMessage::Assistant(blocks) => assistant_to_user_blocks(blocks),
    };
    ChatMessage::User(prefix_sender_label(sender_label, blocks))
}

/// Prepend a `"{label}: "` text block so a peer's message reads as theirs.
/// No-op when the sender name is unknown.
fn prefix_sender_label(label: Option<String>, blocks: Vec<UserContent>) -> Vec<UserContent> {
    let Some(label) = label.filter(|s| !s.is_empty()) else {
        return blocks;
    };
    let mut out = Vec::with_capacity(blocks.len() + 1);
    out.push(UserContent::Text(format!("{label}: ")));
    out.extend(blocks);
    out
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

/// Lift the viewer's own posted blocks from the neutral `User` storage
/// perspective into Assistant content. Posted messages are text; a tool-result
/// block never appears on a `posted` row, so it is dropped defensively.
fn user_to_assistant_blocks(blocks: Vec<UserContent>) -> Vec<AssistantContent> {
    blocks
        .into_iter()
        .filter_map(|b| match b {
            UserContent::Text(t) => Some(AssistantContent::Text(t)),
            // Tool results and attachments never appear on the agent's own
            // posted rows; drop defensively (an attachment is not valid
            // assistant content to replay).
            UserContent::ToolResult(_) | UserContent::Image(_) | UserContent::File(_) => None,
        })
        .collect()
}
