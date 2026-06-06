//! Background scheduler that enqueues reflection turns
//! (doc/memory.md §1.6).
//!
//! Polls Postgres on a configurable cadence. For each `(agent, session)`
//! pair where:
//!
//! 1. the time since the latest message exceeds
//!    [`super::limits::REFLECTION_IDLE_TIMEOUT_SECS`], AND
//! 2. there are messages strictly after the latest
//!    `reflection_checkpoints` row for that pair
//!
//! the scheduler enqueues a single `RequestKind::Reflection` job. The
//! scheduler never talks to the LLM — the worker pool dispatches the
//! resulting row through the same `Agent` path as a normal turn.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::clock::SharedClock;
use crate::provider::{AssistantContent, ChatMessage, UserContent};
use crate::runtime::{
    IdempotencyKey, NewPromptRequest, PromptRequestId, RequestKind, RequestKindPayload,
    RequestStatus, SharedPromptQueue,
};
use crate::session::SessionId;
use crate::tools::truncate_from_start;
use crate::types::{Participant, Prompt};

use crate::scheduling::ScheduledTask;

use super::limits::{
    REFLECTION_IDLE_TIMEOUT_SECS, REFLECTION_SCHEDULER_BATCH_LIMIT, REFLECTION_SCHEDULER_POLL_SECS,
};

#[derive(Debug)]
pub struct ReflectionScheduler {
    task: ScheduledTask,
}

impl ReflectionScheduler {
    /// Spawn with the production poll cadence. The supplied parent token
    /// wires shutdown into the main runtime Ctrl+C signal.
    #[must_use]
    pub fn spawn(
        pool: PgPool,
        queue: SharedPromptQueue,
        clock: SharedClock,
        parent: CancellationToken,
    ) -> Self {
        Self::spawn_with_cadence(
            pool,
            queue,
            clock,
            Duration::from_secs(REFLECTION_SCHEDULER_POLL_SECS),
            Some(parent),
        )
    }

    /// Spawn with an explicit poll cadence. Tests use this to avoid waiting
    /// the production 60s; production callers use [`Self::spawn`].
    #[must_use]
    pub fn spawn_with_cadence(
        pool: PgPool,
        queue: SharedPromptQueue,
        clock: SharedClock,
        poll_interval: Duration,
        parent: Option<CancellationToken>,
    ) -> Self {
        let inner = Arc::new(SchedulerInner {
            pool,
            queue,
            clock,
            idle_threshold: chrono::Duration::seconds(
                i64::try_from(REFLECTION_IDLE_TIMEOUT_SECS)
                    .expect("invariant: REFLECTION_IDLE_TIMEOUT_SECS fits in i64"),
            ),
            batch_limit: REFLECTION_SCHEDULER_BATCH_LIMIT,
        });
        let task = ScheduledTask::spawn("reflection_scheduler", poll_interval, parent, move || {
            let inner = inner.clone();
            async move { inner.tick().await }
        });
        Self { task }
    }

    pub async fn shutdown(self) {
        self.task.shutdown().await;
    }
}

#[derive(Debug)]
struct SchedulerInner {
    pool: PgPool,
    queue: SharedPromptQueue,
    clock: SharedClock,
    idle_threshold: chrono::Duration,
    batch_limit: usize,
}

impl SchedulerInner {
    async fn tick(&self) -> Result<(), sqlx::Error> {
        let now: DateTime<Utc> = self.clock.now_wall().into();
        let cutoff = now - self.idle_threshold;
        let candidates = self.find_candidates(cutoff).await?;

        for c in candidates {
            if let Err(e) = self.enqueue_reflection(&c).await {
                warn!(
                    error = %e,
                    patom.agent.id = %c.agent_id,
                    patom.session.id = %c.session_id,
                    "reflection_scheduler.enqueue.error",
                );
            } else {
                info!(
                    patom.agent.id = %c.agent_id,
                    patom.session.id = %c.session_id,
                    patom.reflection.up_to_turn_id = %c.last_turn_id,
                    "reflection_scheduler.enqueued",
                );
            }
        }
        Ok(())
    }

    /// Find `(agent, session)` pairs whose latest message is older than
    /// `cutoff` and which have at least one message past the most recent
    /// reflection checkpoint (or no checkpoint at all). Excludes pairs
    /// that already have a pending/processing reflection so the scheduler
    /// is idempotent across ticks. The previous checkpoint's `last_turn_id`
    /// returns inline as `previous_cursor` so `enqueue_reflection` doesn't
    /// need a second round-trip.
    #[allow(clippy::type_complexity)]
    async fn find_candidates(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<ReflectionCandidate>, sqlx::Error> {
        // Privileged tx — the scheduler scans across every tenant's
        // agent sessions; RLS on `sessions` / `reflection_checkpoints`
        // / `session_messages` would otherwise hide every row.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows: Vec<(
            AgentId,
            SessionId,
            PromptRequestId,
            DateTime<Utc>,
            Option<PromptRequestId>,
            OrgId,
            UserId,
        )> = sqlx::query_as(
            // The `seed.kind = $6` filter on the session's root prompt_request
            // keeps the scheduler off its own off-DAG reflection sessions —
            // otherwise the scheduler reflects on its previous reflection
            // output, indefinitely, once per idle window. `seed.kind IS NULL`
            // covers test fixtures that mint a session with a synthetic
            // root_request_id; production always inserts the root row.
            //
            // `s.org_id` / `s.created_by_user_id` are carried out so the
            // enqueue path can mint the reflection's off-DAG `(agent,
            // system)` session pinned to the same tenant — the trigger
            // on `sessions` would reject a cross-org child otherwise.
            "WITH latest_per_session AS (
                 SELECT m.session_id,
                        MAX(m.seq) AS latest_seq,
                        MAX(m.created_at) AS latest_at
                 FROM session_messages m
                 GROUP BY m.session_id
             ),
             agent_sessions AS (
                 SELECT id, participant_a_agent_id AS agent_id, root_request_id,
                        org_id, created_by_user_id
                 FROM sessions
                 WHERE participant_a_kind = 'agent'
                 UNION ALL
                 SELECT id, participant_b_agent_id AS agent_id, root_request_id,
                        org_id, created_by_user_id
                 FROM sessions
                 WHERE participant_b_kind = 'agent'
             )
             SELECT a.agent_id,
                    a.id AS session_id,
                    sm.request_id AS last_turn_id,
                    l.latest_at,
                    rc.last_turn_id AS previous_cursor,
                    a.org_id,
                    a.created_by_user_id
             FROM agent_sessions a
             JOIN latest_per_session l ON l.session_id = a.id
             JOIN session_messages sm
                 ON sm.session_id = a.id AND sm.seq = l.latest_seq
             LEFT JOIN reflection_checkpoints rc
                 ON rc.agent_id = a.agent_id AND rc.session_id = a.id
             LEFT JOIN prompt_requests seed
                 ON seed.id = a.root_request_id
             WHERE l.latest_at <= $1
               AND (rc.created_at IS NULL OR rc.created_at < l.latest_at)
               AND (seed.kind IS NULL OR seed.kind = $6)
               AND NOT EXISTS (
                   SELECT 1 FROM prompt_requests pr
                   WHERE pr.kind = $3
                     AND pr.kind_payload->'data'->>'session_id' = a.id::text
                     AND pr.status IN ($4, $5)
               )
             ORDER BY l.latest_at ASC
             LIMIT $2",
        )
        .bind(cutoff)
        .bind(i64::try_from(self.batch_limit).expect("invariant: batch limit fits in i64"))
        .bind(RequestKind::Reflection)
        .bind(RequestStatus::Pending)
        .bind(RequestStatus::Processing)
        .bind(RequestKind::Normal)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    agent_id,
                    session_id,
                    last_turn_id,
                    latest_at,
                    previous_cursor,
                    org_id,
                    created_by_user_id,
                )| {
                    ReflectionCandidate {
                        agent_id,
                        session_id,
                        last_turn_id,
                        latest_at,
                        previous_cursor,
                        org_id,
                        created_by_user_id,
                    }
                },
            )
            .collect())
    }

    /// Enqueue a single reflection job. The idempotency key derives from
    /// `(agent, session, last_turn_id)` so a candidate that survives across
    /// two ticks (because the previous enqueue is still pending) maps back
    /// to the same row.
    async fn enqueue_reflection(&self, c: &ReflectionCandidate) -> Result<(), EnqueueError> {
        let viewer = Participant::agent(c.agent_id);
        let key = IdempotencyKey::try_from(format!(
            "reflect-{agent}-{session}-{turn}",
            agent = c.agent_id,
            session = c.session_id,
            turn = c.last_turn_id,
        ))
        .expect("invariant: reflection idempotency key fits the cap");

        let slice = self
            .fetch_slice(c.session_id, c.agent_id, c.previous_cursor, c.last_turn_id)
            .await?;
        let content = build_reflection_prompt(&slice);

        // `parent_session: None` keeps the reflection session off the
        // conversation DAG so its trace cannot leak back into the parent.
        let req = NewPromptRequest {
            session: None,
            sender: viewer,
            receiver_agent_id: c.agent_id,
            parent_session: None,
            content,
            idempotency_key: key,
            org_id: c.org_id,
            created_by_user_id: c.created_by_user_id,
            kind_payload: RequestKindPayload::Reflection {
                session_id: c.session_id,
                up_to_turn_id: c.last_turn_id,
            },
        };
        let outcome = self.queue.enqueue(req).await?;
        debug!(
            patom.request.id = %outcome.request_id(),
            "reflection_scheduler.enqueued.row",
        );
        Ok(())
    }

    /// Fetch viewer-mapped messages in the conversation session whose `seq`
    /// is in `(previous_cursor, up_to]`. Returns rows in seq-ascending
    /// order. When `previous_cursor` is `None` (first reflection), the
    /// lower bound is treated as -1 — every row up to and including
    /// `up_to`'s seq is returned.
    async fn fetch_slice(
        &self,
        conversation: SessionId,
        agent: AgentId,
        previous_cursor: Option<PromptRequestId>,
        up_to: PromptRequestId,
    ) -> Result<Vec<ChatMessage>, EnqueueError> {
        let viewer_agent_id = agent.as_uuid();
        // Privileged tx: `session_messages` is RLS-forced; the
        // scheduler scans tenant-wide.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows: Vec<(Option<uuid::Uuid>, serde_json::Value)> = sqlx::query_as(
            "WITH bounds AS (
                 SELECT
                     COALESCE(
                         (SELECT MAX(seq) FROM session_messages
                          WHERE session_id = $1 AND request_id = $2),
                         -1
                     ) AS low,
                     COALESCE(
                         (SELECT MAX(seq) FROM session_messages
                          WHERE session_id = $1 AND request_id = $3),
                         -1
                     ) AS high
             )
             SELECT m.sender_agent_id, m.body
             FROM session_messages m, bounds
             WHERE m.session_id = $1
               AND m.seq > bounds.low
               AND m.seq <= bounds.high
             ORDER BY m.seq ASC",
        )
        .bind(conversation)
        .bind(previous_cursor)
        .bind(up_to)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut out = Vec::with_capacity(rows.len());
        for (sender_agent_id, body) in rows {
            let stored: ChatMessage = serde_json::from_value(body)?;
            let is_self = sender_agent_id == Some(viewer_agent_id);
            out.push(map_for_viewer(stored, is_self));
        }
        Ok(out)
    }
}

/// Reflection-scheduler-local error sum. Postgres reads, message-body decode,
/// and queue enqueue failures all funnel through one type so `tick` logs them
/// at the same callsite.
#[derive(Debug, thiserror::Error)]
enum EnqueueError {
    #[error("postgres: {0}")]
    Db(#[from] sqlx::Error),
    #[error("decode session_messages body: {0}")]
    Decode(#[from] serde_json::Error),
    #[error(transparent)]
    Queue(#[from] crate::runtime::PromptError),
}

/// Build the reflection-turn user prompt from the captured slice. The
/// system prompt frames the job; this body is the transcript itself,
/// trimmed from the head when oversized so the most recent turns survive.
fn build_reflection_prompt(slice: &[ChatMessage]) -> Prompt {
    const HEADER: &str = "Reflect on the conversation below. \
        Identify what should be remembered, updated, or forgotten.\n\n\
        ## Conversation\n";
    const NOTICE: &str = "[earlier turns truncated to fit prompt cap]\n";
    const FALLBACK: &str = "Reflect on this session. No new turns are available since the last \
        checkpoint; review existing memory instead.";

    let mut transcript = String::new();
    for message in slice {
        render_chat_message(message, &mut transcript);
    }
    if transcript.trim().is_empty() {
        return Prompt::try_from(FALLBACK).expect("invariant: fallback fits Prompt cap");
    }

    let cap = crate::types::PROMPT_MAX_BYTES;
    let body = if HEADER.len() + transcript.len() <= cap {
        let mut s = String::with_capacity(HEADER.len() + transcript.len());
        s.push_str(HEADER);
        s.push_str(&transcript);
        s
    } else {
        let max_transcript = cap.saturating_sub(HEADER.len() + NOTICE.len());
        let trimmed = truncate_from_start(&transcript, max_transcript);
        let mut s = String::with_capacity(HEADER.len() + NOTICE.len() + trimmed.len());
        s.push_str(HEADER);
        s.push_str(NOTICE);
        s.push_str(trimmed);
        s
    };
    Prompt::try_from(body).expect("invariant: body trimmed to Prompt cap")
}

/// Render a `ChatMessage` as a single transcript line. `is_self` flips
/// the role label so the agent reading the prompt sees its own past
/// turns labelled `Assistant:` and the other side's labelled `User:`.
fn render_chat_message(message: &ChatMessage, out: &mut String) {
    match message {
        ChatMessage::User(blocks) => {
            out.push_str("User: ");
            for block in blocks {
                match block {
                    UserContent::Text(t) => out.push_str(t),
                    UserContent::ToolResult(r) => {
                        out.push_str("[tool-result ");
                        out.push_str(r.call_id.as_str());
                        out.push_str(if r.is_error { ": err]" } else { ": ok]" });
                    }
                }
            }
            out.push('\n');
        }
        ChatMessage::Assistant(blocks) => {
            out.push_str("Assistant: ");
            for block in blocks {
                match block {
                    AssistantContent::Text(t) | AssistantContent::Reasoning(t) => {
                        out.push_str(t);
                    }
                    AssistantContent::ToolCall(c) => {
                        out.push_str("[tool-call ");
                        out.push_str(c.name.as_str());
                        out.push('(');
                        out.push_str(&c.input.to_string());
                        out.push_str(")]");
                    }
                }
            }
            out.push('\n');
        }
    }
}

/// Map a stored `ChatMessage` to the reflecting agent's perspective.
/// `is_self == true` keeps the assistant variant for own turns; the other
/// side's rows flip into the user-content shape so the transcript labels
/// them as `User:` consistently.
fn map_for_viewer(stored: ChatMessage, is_self: bool) -> ChatMessage {
    match (is_self, stored) {
        (true, msg @ ChatMessage::Assistant(_)) | (false, msg @ ChatMessage::User(_)) => msg,
        (true, ChatMessage::User(blocks)) => {
            let assist = blocks
                .into_iter()
                .map(|b| match b {
                    UserContent::Text(t) => AssistantContent::Text(t),
                    UserContent::ToolResult(r) => AssistantContent::Text(format!(
                        "[tool-result {}: {}]",
                        r.call_id.as_str(),
                        if r.is_error { "err" } else { "ok" }
                    )),
                })
                .collect();
            ChatMessage::Assistant(assist)
        }
        (false, ChatMessage::Assistant(blocks)) => {
            let user = blocks
                .into_iter()
                .map(|b| match b {
                    AssistantContent::Text(t) | AssistantContent::Reasoning(t) => {
                        UserContent::Text(t)
                    }
                    AssistantContent::ToolCall(c) => {
                        UserContent::Text(format!("[tool-call {}({})]", c.name.as_str(), c.input))
                    }
                })
                .collect();
            ChatMessage::User(user)
        }
    }
}

#[derive(Debug, Clone)]
struct ReflectionCandidate {
    agent_id: AgentId,
    session_id: SessionId,
    /// Latest message at scheduler time — the upper end of the slice.
    last_turn_id: PromptRequestId,
    /// Latest message timestamp; used only by the SQL ordering and surfaced
    /// in tracing, not read by the enqueue path.
    #[allow(dead_code)]
    latest_at: DateTime<Utc>,
    /// Lower end of the slice from the existing checkpoint, joined inline
    /// by `find_candidates`. `None` on the first reflection.
    previous_cursor: Option<PromptRequestId>,
    /// Owning org of the conversation session; the reflection's off-DAG
    /// `(agent, system)` session inherits it.
    org_id: OrgId,
    /// User the conversation session is attributed to. The reflection
    /// runs against the same principal so RLS-bound reads inside the
    /// worker turn see the right scope.
    created_by_user_id: UserId,
}

// SQL paths are covered by `tests/reflection_pipeline.rs` against a real
// Postgres. The candidate struct is too trivial to merit pure-unit tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCall, ToolCallId, ToolResult};
    use crate::types::ToolName;
    use serde_json::json;

    fn tool_call_id(s: &str) -> ToolCallId {
        ToolCallId::try_from(s).expect("invariant: literal tool-call id is valid")
    }

    fn tool_name(s: &str) -> ToolName {
        ToolName::try_from(s).expect("invariant: literal tool name is valid")
    }

    #[test]
    fn tool_result_body_dropped_from_self_user_block() {
        let big = "x".repeat(100_000);
        let msg = ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: tool_call_id("call-1"),
            output: big.clone(),
            is_error: false,
        })]);
        let mut out = String::new();
        render_chat_message(&msg, &mut out);
        assert!(out.contains("[tool-result call-1: ok]"));
        assert!(!out.contains(&big));
        assert!(out.len() < 200);
    }

    #[test]
    fn tool_result_body_dropped_from_other_assistant_block() {
        let big = "x".repeat(100_000);
        let stored = ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: tool_call_id("call-2"),
            output: big.clone(),
            is_error: false,
        })]);
        let flipped = map_for_viewer(stored, false);
        let mut out = String::new();
        render_chat_message(&flipped, &mut out);
        assert!(out.contains("[tool-result call-2: ok]"));
        assert!(!out.contains(&big));
        assert!(out.len() < 200);
    }

    #[test]
    fn tool_result_error_marker() {
        let msg = ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: tool_call_id("call-3"),
            output: "boom".into(),
            is_error: true,
        })]);
        let mut self_out = String::new();
        render_chat_message(&msg, &mut self_out);
        assert!(self_out.contains("[tool-result call-3: err]"));
        assert!(!self_out.contains("boom"));

        let flipped = map_for_viewer(msg, false);
        let mut other_out = String::new();
        render_chat_message(&flipped, &mut other_out);
        assert!(other_out.contains("[tool-result call-3: err]"));
        assert!(!other_out.contains("boom"));
    }

    #[test]
    fn reasoning_and_tool_call_args_preserved_verbatim() {
        let assistant = ChatMessage::Assistant(vec![
            AssistantContent::Reasoning("digested findings".into()),
            AssistantContent::ToolCall(ToolCall {
                id: tool_call_id("call-4"),
                name: tool_name("send_message"),
                input: json!({"text": "report"}),
            }),
        ]);

        let mut self_out = String::new();
        render_chat_message(&assistant, &mut self_out);
        assert!(self_out.contains("digested findings"));
        assert!(self_out.contains("[tool-call send_message("));
        assert!(self_out.contains(r#""text":"report""#));

        let flipped = map_for_viewer(assistant, false);
        let mut other_out = String::new();
        render_chat_message(&flipped, &mut other_out);
        assert!(other_out.contains("digested findings"));
        assert!(other_out.contains("[tool-call send_message("));
        assert!(other_out.contains(r#""text":"report""#));
    }

    #[test]
    fn large_web_fetch_no_longer_triggers_truncation_notice() {
        let huge = "x".repeat(200_000);
        let slice = vec![
            ChatMessage::User(vec![UserContent::Text("Produce the daily report".into())]),
            ChatMessage::Assistant(vec![
                AssistantContent::Reasoning("Let me fetch the index page.".into()),
                AssistantContent::ToolCall(ToolCall {
                    id: tool_call_id("call-5"),
                    name: tool_name("web_fetch"),
                    input: json!({"url": "https://example.com"}),
                }),
            ]),
            ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
                call_id: tool_call_id("call-5"),
                output: huge.clone(),
                is_error: false,
            })]),
            ChatMessage::Assistant(vec![AssistantContent::Text("Done.".into())]),
        ];

        let prompt = build_reflection_prompt(&slice);
        let body = prompt.as_str();
        assert!(
            !body.contains("[earlier turns truncated to fit prompt cap]"),
            "prompt unexpectedly head-trimmed: {} bytes",
            body.len()
        );
        assert!(body.contains("[tool-result call-5: ok]"));
        assert!(!body.contains(&huge));
    }
}
