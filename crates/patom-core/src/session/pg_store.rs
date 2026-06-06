//! Postgres-backed [`SessionStore`].
//!
//! Conversation history is stored in `session_messages (session_id, seq,
//! sender_*, receiver_*, body JSONB, created_at)`. The body column carries
//! the full [`ChatMessage`] envelope as JSONB so adding a content variant is a
//! code change, not a migration. Per-session ordering is provided by the `seq`
//! column, assigned monotonically inside `append`.
//!
//! Wall-clock timestamps come from the injected [`SharedClock`] — never
//! `NOW()` in app SQL — so `TestClock`-driven tests see stable timestamps
//! (CLAUDE.md §11).

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::provider::{AssistantContent, ChatMessage, UserContent};
use crate::runtime::PromptRequestId;
use crate::types::{
    MessageSender, MessageSenderKind, Participant, ParticipantDecodeError, ParticipantKind,
};

use super::error::SessionError;
use super::limits::MAX_MESSAGES_PER_SESSION;
use super::traits::{SessionId, SessionStore, SessionTenancy};

/// Postgres-backed [`SessionStore`]. Holds a cheap clone of a [`PgPool`] and a
/// [`SharedClock`]; safe to share across the runtime.
pub struct PgSessionStore {
    pool: PgPool,
    clock: SharedClock,
    message_cap: u32,
}

impl PgSessionStore {
    /// Construct a store backed by `pool`, using `clock` for every wall-clock value.
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self::with_caps(pool, clock, MAX_MESSAGES_PER_SESSION)
    }

    #[must_use]
    pub fn with_caps(pool: PgPool, clock: SharedClock, message_cap: u32) -> Self {
        Self {
            pool,
            clock,
            message_cap,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSessionStore")
            .field("message_cap", &self.message_cap)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SessionStore for PgSessionStore {
    #[tracing::instrument(
        skip_all,
        name = "session.resolve_or_create",
        fields(
            patom.dag.root = %root_request_id,
            patom.parent.session.id = parent_session_id.map(tracing::field::display),
            patom.org.id = %org_id,
            patom.session.id = tracing::field::Empty,
            patom.session.created = tracing::field::Empty,
        ),
    )]
    async fn resolve_or_create_for_pair(
        &self,
        root_request_id: PromptRequestId,
        a: Participant,
        b: Participant,
        parent_session_id: Option<SessionId>,
        org_id: OrgId,
        created_by_user_id: UserId,
    ) -> Result<SessionId, SessionError> {
        // Privileged tx: the store is reachable from both
        // tenant-gated HTTP handlers and from infrastructure paths
        // (the queue's implicit-session-create) that lack a principal.
        // Tenant scoping is provided by the explicit `org_id` bound
        // on every statement and by the trigger that pins a child
        // session's org to its parent.
        run_privileged(&self.pool, async |tx| {
            resolve_or_create_for_pair_inner(
                self,
                tx.tx_mut(),
                root_request_id,
                a,
                b,
                parent_session_id,
                org_id,
                created_by_user_id,
            )
            .await
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "session.resolve_or_create_for_user",
        fields(
            patom.dag.root = %root_request_id,
            patom.parent.session.id = parent_session_id.map(tracing::field::display),
            patom.org.id = %caller.org_id,
            patom.user.id = %caller.user_id,
            patom.session.id = tracing::field::Empty,
            patom.session.created = tracing::field::Empty,
        ),
    )]
    async fn resolve_or_create_for_pair_for_user(
        &self,
        caller: &Caller,
        root_request_id: PromptRequestId,
        a: Participant,
        b: Participant,
        parent_session_id: Option<SessionId>,
    ) -> Result<SessionId, SessionError> {
        // Tenant-scoped tx — the RLS WITH CHECK on `sessions.org_id`
        // rejects a row whose `org_id` is not in the acting user's
        // memberships. Worker / tool callers source `caller` from the
        // claimed session's `(created_by_user_id, org_id)`.
        //
        // Identity invariant: the inserted session's `created_by_user_id`
        // is the authenticated actor, never a free-form caller-supplied
        // value. `Caller` makes this impossible to express incorrectly.
        run_as_user(&self.pool, caller.user_id, async |tx| {
            resolve_or_create_for_pair_inner(
                self,
                tx.tx_mut(),
                root_request_id,
                a,
                b,
                parent_session_id,
                caller.org_id,
                caller.user_id,
            )
            .await
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "session.append",
        fields(
            patom.session.id = %id,
            patom.message.kind = chat_message_kind(&message),
            patom.message.blocks = chat_message_block_count(&message),
        ),
    )]
    async fn append(
        &self,
        id: SessionId,
        sender: MessageSender,
        receiver: Participant,
        message: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError> {
        run_privileged(&self.pool, async |tx| {
            append_row(self, tx.tx_mut(), id, sender, receiver, message, request_id).await
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "session.append_for_user",
        fields(
            patom.session.id = %id,
            patom.user.id = %acting_user_id,
            patom.message.kind = chat_message_kind(&message),
            patom.message.blocks = chat_message_block_count(&message),
        ),
    )]
    async fn append_for_user(
        &self,
        acting_user_id: UserId,
        id: SessionId,
        sender: MessageSender,
        receiver: Participant,
        message: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            append_row(self, tx.tx_mut(), id, sender, receiver, message, request_id).await
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "session.append_system_nudge",
        fields(
            patom.session.id = %id,
            patom.bytes = note.len(),
        ),
    )]
    async fn append_system_nudge(
        &self,
        id: SessionId,
        receiver: Participant,
        note: String,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError> {
        // The system note is stored as a single-text user content block
        // so the viewer-mapped snapshot folds it into the receiver's
        // prompt as user-side context — exactly how a system reminder
        // renders to the model.
        let body = ChatMessage::User(vec![UserContent::Text(note)]);
        run_privileged(&self.pool, async |tx| {
            append_row(
                self,
                tx.tx_mut(),
                id,
                MessageSender::System,
                receiver,
                body,
                request_id,
            )
            .await
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "session.append_system_nudge_for_user",
        fields(
            patom.session.id = %id,
            patom.user.id = %acting_user_id,
            patom.bytes = note.len(),
        ),
    )]
    async fn append_system_nudge_for_user(
        &self,
        acting_user_id: UserId,
        id: SessionId,
        receiver: Participant,
        note: String,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError> {
        let body = ChatMessage::User(vec![UserContent::Text(note)]);
        run_as_user(&self.pool, acting_user_id, async |tx| {
            append_row(
                self,
                tx.tx_mut(),
                id,
                MessageSender::System,
                receiver,
                body,
                request_id,
            )
            .await
        })
        .await
    }

    // TODO: revisit when we need tuning prompt, currently we attach
    // both assistant and user response for clarity.
    // However, it could make context size float with unnecessary information.
    #[tracing::instrument(
        skip_all,
        name = "session.snapshot",
        fields(
            patom.session.id = %id,
            patom.viewer = %viewer,
            patom.history.count = tracing::field::Empty,
        ),
    )]
    async fn snapshot(
        &self,
        id: SessionId,
        viewer: Participant,
    ) -> Result<Vec<ChatMessage>, SessionError> {
        type Row = (
            MessageSenderKind,
            Option<AgentId>,
            ParticipantKind,
            Option<AgentId>,
            serde_json::Value,
        );
        let rows: Vec<Row> =
            run_privileged::<Option<Vec<Row>>, SessionError>(&self.pool, async |tx| {
                let exists: Option<(SessionId,)> =
                    sqlx::query_as("SELECT id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?;
                if exists.is_none() {
                    return Ok(None);
                }
                let rows: Vec<Row> = sqlx::query_as(
                    "SELECT sender_kind, sender_agent_id,
                            receiver_kind, receiver_agent_id,
                            body
                         FROM session_messages
                         WHERE session_id = $1
                         ORDER BY seq ASC",
                )
                .bind(id)
                .fetch_all(&mut **tx)
                .await?;
                Ok(Some(rows))
            })
            .await?
            .ok_or(SessionError::NotFound(id))?;

        tracing::Span::current().record("patom.history.count", rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for (sender_kind, sender_agent_id, receiver_kind, receiver_agent_id, body) in rows {
            let sender = MessageSender::from_kind_id(sender_kind, sender_agent_id)
                .map_err(invariant_to_backend)?;
            let receiver = Participant::from_kind_id(receiver_kind, receiver_agent_id)
                .map_err(invariant_to_backend)?;
            let stored: ChatMessage = serde_json::from_value(body).map_err(|e| {
                SessionError::Backend(format!("deserialize message for session {id:?}: {e}"))
            })?;
            out.push(map_message_for_viewer(stored, sender, receiver, viewer));
        }
        Ok(out)
    }

    #[tracing::instrument(
        skip_all,
        name = "session.snapshot_window",
        fields(
            patom.session.id = %id,
            patom.viewer = %viewer,
            patom.window.limit = limit,
            patom.window.before_seq = before_seq.map(tracing::field::display),
            patom.history.count = tracing::field::Empty,
        ),
    )]
    async fn snapshot_window(
        &self,
        id: SessionId,
        viewer: Participant,
        limit: u32,
        before_seq: Option<i64>,
    ) -> Result<Vec<(i64, ChatMessage)>, SessionError> {
        type WindowRow = (
            i64,
            MessageSenderKind,
            Option<AgentId>,
            ParticipantKind,
            Option<AgentId>,
            serde_json::Value,
        );
        // Pull the newest `limit` rows below `before_seq` (or the
        // newest overall when no cursor is set), then re-order to seq
        // ASC for the caller. Postgres LIMIT requires a BIGINT.
        let limit_i64 = i64::from(limit);
        let cursor = before_seq.unwrap_or(i64::MAX);
        let mut rows =
            run_privileged::<Option<Vec<WindowRow>>, SessionError>(&self.pool, async |tx| {
                let exists: Option<(SessionId,)> =
                    sqlx::query_as("SELECT id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?;
                if exists.is_none() {
                    return Ok(None);
                }
                let rows: Vec<WindowRow> = sqlx::query_as(
                    "SELECT seq, sender_kind, sender_agent_id,
                            receiver_kind, receiver_agent_id,
                            body
                         FROM session_messages
                         WHERE session_id = $1 AND seq < $2
                         ORDER BY seq DESC
                         LIMIT $3",
                )
                .bind(id)
                .bind(cursor)
                .bind(limit_i64)
                .fetch_all(&mut **tx)
                .await?;
                Ok(Some(rows))
            })
            .await?
            .ok_or(SessionError::NotFound(id))?;
        rows.reverse();

        tracing::Span::current().record("patom.history.count", rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for (seq, sender_kind, sender_agent_id, receiver_kind, receiver_agent_id, body) in rows {
            let sender = MessageSender::from_kind_id(sender_kind, sender_agent_id)
                .map_err(invariant_to_backend)?;
            let receiver = Participant::from_kind_id(receiver_kind, receiver_agent_id)
                .map_err(invariant_to_backend)?;
            let stored: ChatMessage = serde_json::from_value(body).map_err(|e| {
                SessionError::Backend(format!("deserialize message for session {id:?}: {e}"))
            })?;
            out.push((
                seq,
                map_message_for_viewer(stored, sender, receiver, viewer),
            ));
        }
        Ok(out)
    }

    #[tracing::instrument(
        skip_all,
        name = "session.participants",
        fields(patom.session.id = %id),
    )]
    async fn participants(
        &self,
        id: SessionId,
    ) -> Result<(Participant, Participant), SessionError> {
        type Row = (
            ParticipantKind,
            Option<AgentId>,
            ParticipantKind,
            Option<AgentId>,
        );
        let row = run_privileged::<Option<Row>, SessionError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT participant_a_kind, participant_a_agent_id,
                        participant_b_kind, participant_b_agent_id
                 FROM sessions
                 WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;

        let (ak, aid, bk, bid) = row.ok_or(SessionError::NotFound(id))?;
        let a = Participant::from_kind_id(ak, aid).map_err(invariant_to_backend)?;
        let b = Participant::from_kind_id(bk, bid).map_err(invariant_to_backend)?;
        Ok((a, b))
    }

    #[tracing::instrument(
        skip_all,
        name = "session.parent",
        fields(patom.session.id = %id),
    )]
    async fn parent(&self, id: SessionId) -> Result<Option<SessionId>, SessionError> {
        let row =
            run_privileged::<Option<(Option<SessionId>,)>, SessionError>(&self.pool, async |tx| {
                Ok(
                    sqlx::query_as("SELECT parent_session_id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?,
                )
            })
            .await?;
        let (parent,) = row.ok_or(SessionError::NotFound(id))?;
        Ok(parent)
    }

    /// One round-trip: the agent-loop hot path used to fan out into `parent`
    /// + `participants` + `snapshot` (3 RTTs every turn). The CTE pins the
    /// parent session, applies the participant predicate inline, and the
    /// final SELECT joins through `session_messages`. `IS NOT DISTINCT FROM`
    /// matches both `(agent, agent_id)` and `(human, NULL)` correctly.
    #[tracing::instrument(
        skip_all,
        name = "session.parent_history_for_viewer",
        fields(
            patom.session.id = %id,
            patom.viewer = %viewer,
            patom.history.count = tracing::field::Empty,
        ),
    )]
    async fn parent_history_for_viewer(
        &self,
        id: SessionId,
        viewer: Participant,
    ) -> Result<Vec<ChatMessage>, SessionError> {
        type Row = (
            MessageSenderKind,
            Option<AgentId>,
            ParticipantKind,
            Option<AgentId>,
            serde_json::Value,
        );
        let rows = run_privileged::<Vec<Row>, SessionError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "WITH parent_session AS (
                     SELECT s.id AS parent_id
                     FROM sessions cur
                     JOIN sessions s ON s.id = cur.parent_session_id
                     WHERE cur.id = $1
                       AND (
                           (s.participant_a_kind = $2 AND s.participant_a_agent_id IS NOT DISTINCT FROM $3)
                        OR (s.participant_b_kind = $2 AND s.participant_b_agent_id IS NOT DISTINCT FROM $3)
                       )
                 )
                 SELECT m.sender_kind, m.sender_agent_id,
                        m.receiver_kind, m.receiver_agent_id,
                        m.body
                 FROM session_messages m
                 JOIN parent_session ps ON m.session_id = ps.parent_id
                 ORDER BY m.seq ASC",
            )
            .bind(id)
            .bind(viewer.kind())
            .bind(viewer.agent_id())
            .fetch_all(&mut **tx)
            .await?)
        })
        .await?;

        tracing::Span::current().record("patom.history.count", rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for (sender_kind, sender_agent_id, receiver_kind, receiver_agent_id, body) in rows {
            let sender = MessageSender::from_kind_id(sender_kind, sender_agent_id)
                .map_err(invariant_to_backend)?;
            let receiver = Participant::from_kind_id(receiver_kind, receiver_agent_id)
                .map_err(invariant_to_backend)?;
            let stored: ChatMessage = serde_json::from_value(body).map_err(|e| {
                SessionError::Backend(format!("deserialize parent message for {id:?}: {e}"))
            })?;
            out.push(map_message_for_viewer(stored, sender, receiver, viewer));
        }
        Ok(out)
    }

    #[tracing::instrument(
        skip_all,
        name = "session.root_request_id",
        fields(patom.session.id = %id),
    )]
    async fn root_request_id(&self, id: SessionId) -> Result<PromptRequestId, SessionError> {
        let row =
            run_privileged::<Option<(PromptRequestId,)>, SessionError>(&self.pool, async |tx| {
                Ok(
                    sqlx::query_as("SELECT root_request_id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?,
                )
            })
            .await?;
        let (root,) = row.ok_or(SessionError::NotFound(id))?;
        Ok(root)
    }

    #[tracing::instrument(
        skip_all,
        name = "session.tenancy",
        fields(patom.session.id = %id),
    )]
    async fn tenancy(&self, id: SessionId) -> Result<SessionTenancy, SessionError> {
        let row = run_privileged::<Option<(OrgId, UserId)>, SessionError>(&self.pool, async |tx| {
            Ok(
                sqlx::query_as("SELECT org_id, created_by_user_id FROM sessions WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?,
            )
        })
        .await?;
        let (org_id, created_by_user_id) = row.ok_or(SessionError::NotFound(id))?;
        Ok(SessionTenancy {
            org_id,
            created_by_user_id,
        })
    }

    #[tracing::instrument(
        skip_all,
        name = "session.delete",
        fields(patom.session.id = %id),
    )]
    async fn delete(&self, id: SessionId) -> Result<(), SessionError> {
        let rows_affected = run_privileged::<u64, SessionError>(&self.pool, async |tx| {
            let res = sqlx::query("DELETE FROM sessions WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            Ok(res.rows_affected())
        })
        .await?;
        if rows_affected == 0 {
            return Err(SessionError::NotFound(id));
        }
        Ok(())
    }
}

/// Shared body of `resolve_or_create_for_pair` (privileged) and
/// `resolve_or_create_for_pair_for_user` (tenant-scoped). The caller
/// opens the transaction and commits / rolls back; this helper only
/// runs the INSERT … ON CONFLICT statement so the same SQL is reused
/// across both entry points.
#[allow(clippy::too_many_arguments)]
async fn resolve_or_create_for_pair_inner(
    store: &PgSessionStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    root_request_id: PromptRequestId,
    a: Participant,
    b: Participant,
    parent_session_id: Option<SessionId>,
    org_id: OrgId,
    created_by_user_id: UserId,
) -> Result<SessionId, SessionError> {
    // §1: parse, don't validate — canonicalise inside the store so
    // a caller cannot accidentally insert the reversed-order row.
    let (a, b) = Participant::canonical_pair(a, b).ok_or(SessionError::SelfSession)?;
    let now = DateTime::<Utc>::from(store.clock.now_wall());
    let new_id = SessionId::new();

    // The unique index `sessions_dag_pair_unique` is on
    // (org_id, root_request_id, a*, b*), and `xmax = 0` distinguishes a
    // fresh insert from a hit on the existing row. We always
    // RETURNING `id` so the caller gets the canonical row id
    // either way.
    let (id, inserted): (SessionId, bool) = sqlx::query_as(
        "INSERT INTO sessions
             (id, created_at, org_id, created_by_user_id,
              parent_session_id, root_request_id,
              participant_a_kind, participant_a_agent_id,
              participant_b_kind, participant_b_agent_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (org_id, root_request_id,
                      participant_a_kind, participant_a_agent_id,
                      participant_b_kind, participant_b_agent_id)
             DO UPDATE SET id = sessions.id
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(new_id)
    .bind(now)
    .bind(org_id)
    .bind(created_by_user_id)
    .bind(parent_session_id)
    .bind(root_request_id)
    .bind(a.kind())
    .bind(a.agent_id())
    .bind(b.kind())
    .bind(b.agent_id())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_agent_fk)?;

    let span = tracing::Span::current();
    span.record("patom.session.id", tracing::field::display(id));
    span.record("patom.session.created", inserted);
    Ok(id)
}

/// Single-row insert path shared by `append` and `append_system_nudge`
/// in both their privileged and tenant-scoped flavours.
///
/// One round-trip: a CTE locks the session row (`FOR UPDATE` serialises
/// concurrent appends), computes `next_seq`/`row_count` against the
/// already-locked snapshot, runs the data-modifying `INSERT … SELECT … WHERE
/// row_count < cap`, and reports back which gate fired (no session, cap hit,
/// or success). The single statement is its own implicit transaction so the
/// row lock is held for exactly the read-modify-write window — same
/// concurrency story as the prior `BEGIN; SELECT FOR UPDATE; SELECT MAX/COUNT;
/// INSERT; COMMIT;` sequence, but at one third the round-trips.
#[allow(clippy::too_many_arguments)]
async fn append_row(
    store: &PgSessionStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: SessionId,
    sender: MessageSender,
    receiver: Participant,
    message: ChatMessage,
    request_id: PromptRequestId,
) -> Result<(), SessionError> {
    let now = store.now();
    let body = serde_json::to_value(&message).map_err(|e| {
        // §12: serialization failure is a backend invariant, not a user error.
        SessionError::Backend(format!("serialize message: {e}"))
    })?;
    let cap = store.message_cap;
    let cap_i64 = i64::from(cap);

    // The trigger on `session_messages` raises if our bound `org_id`
    // ever drifts from the parent session's; RLS adds the cross-tenant
    // defence on top.
    let row: Option<(bool, i64)> = sqlx::query_as(
        "WITH locked AS (
             SELECT id, org_id FROM sessions WHERE id = $1 FOR UPDATE
         ),
         stats AS (
             SELECT
                 (SELECT COUNT(*) FROM session_messages WHERE session_id = $1)
                     AS row_count,
                 (SELECT COALESCE(MAX(seq) + 1, 0) FROM session_messages WHERE session_id = $1)
                     AS next_seq
         ),
         inserted AS (
             INSERT INTO session_messages
                 (session_id, seq,
                  sender_kind, sender_agent_id,
                  receiver_kind, receiver_agent_id,
                  body, created_at, request_id, org_id)
             SELECT $1, stats.next_seq, $3, $4, $5, $6, $7, $8, $9,
                    (SELECT org_id FROM locked)
             FROM stats
             WHERE stats.row_count < $2
               AND EXISTS (SELECT 1 FROM locked)
             RETURNING seq
         )
         SELECT
             EXISTS (SELECT 1 FROM inserted) AS inserted,
             stats.row_count
         FROM stats
         WHERE EXISTS (SELECT 1 FROM locked)",
    )
    .bind(id)
    .bind(cap_i64)
    .bind(sender.kind())
    .bind(sender.agent_id())
    .bind(receiver.kind())
    .bind(receiver.agent_id())
    .bind(body)
    .bind(now)
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_agent_fk)?;

    let Some((inserted, row_count)) = row else {
        // Outer SELECT had no rows ⇒ the `locked` CTE found no session row
        // (`FOR UPDATE` matched nothing).
        return Err(SessionError::NotFound(id));
    };
    if !inserted {
        // Lock acquired, but the cap predicate rejected the INSERT. The
        // returned `row_count` is the live count at the moment of the lock —
        // assert the invariant the SQL guarantees.
        assert!(
            row_count >= cap_i64,
            "invariant: insert skipped only when row_count >= cap",
        );
        return Err(SessionError::MessageCapExceeded { id, max: cap });
    }
    Ok(())
}

/// Render a stored message from `viewer`'s perspective.
///
/// `sender == viewer` ⇒ assistant; everything else ⇒ user. The stored
/// `ChatMessage` already has its content split into User/Assistant variants;
/// we re-tag without altering the content blocks.
///
/// `receiver` is the row's addressee — needed to decide whether a
/// `ToolResult` block belongs to the viewer's own tool call or to the other
/// side's. In an agent↔agent session both parties' tool-result rows share
/// `sender == System`, so receiver is the only field that disambiguates them.
/// The other side's `Assistant.tool_calls` have already been collapsed to
/// text in the `(false, Assistant)` arm, losing the `tool_call_id`; the
/// matching tool-result must be collapsed too or the wire payload is invalid.
fn map_message_for_viewer(
    stored: ChatMessage,
    sender: MessageSender,
    receiver: Participant,
    viewer: Participant,
) -> ChatMessage {
    let is_self = match (sender, viewer) {
        (MessageSender::Human, Participant::Human) => true,
        (MessageSender::Agent { agent_id: a }, Participant::Agent { agent_id: v }) => a == v,
        _ => false,
    };

    match (is_self, stored) {
        (true, ChatMessage::Assistant(blocks)) => ChatMessage::Assistant(blocks),
        (true, ChatMessage::User(blocks)) => {
            // Rare path: a row stored as User-content but written by viewer
            // (e.g. older test scaffolding). Round-trip the text blocks into
            // assistant-text so the snapshot is self-consistent. Tool-result
            // blocks have no assistant analogue; drop them with a marker text
            // so the model still sees they happened.
            ChatMessage::Assistant(user_to_assistant_blocks(blocks))
        }
        (false, ChatMessage::Assistant(blocks)) => {
            // Author was not viewer: render the assistant's text/tool-call blocks
            // back as user-facing context. Tool calls round-trip as text so the
            // viewer sees what the other side did.
            ChatMessage::User(assistant_to_user_blocks(blocks))
        }
        (false, ChatMessage::User(blocks)) => {
            ChatMessage::User(retag_other_side_user_blocks(blocks, receiver, viewer))
        }
    }
}

/// On a `(false, User)` row, fold any `ToolResult` whose matching tool call
/// did not survive viewer-mapping into a marker `Text` block. A row's
/// tool-result belongs to the viewer's own tool call iff `receiver == viewer`;
/// anything else came from the other side, whose `Assistant.tool_calls` were
/// already collapsed to text in the sibling arm.
fn retag_other_side_user_blocks(
    blocks: Vec<UserContent>,
    receiver: Participant,
    viewer: Participant,
) -> Vec<UserContent> {
    if receiver == viewer {
        return blocks;
    }
    blocks
        .into_iter()
        .map(|b| match b {
            UserContent::Text(t) => UserContent::Text(t),
            UserContent::ToolResult(r) => UserContent::Text(format!(
                "[tool-result {}: {}]",
                r.call_id.as_str(),
                r.output
            )),
        })
        .collect()
}

fn user_to_assistant_blocks(blocks: Vec<UserContent>) -> Vec<AssistantContent> {
    blocks
        .into_iter()
        .map(|b| match b {
            UserContent::Text(t) => AssistantContent::Text(t),
            UserContent::ToolResult(r) => AssistantContent::Text(format!(
                "[tool-result {}: {}]",
                r.call_id.as_str(),
                r.output
            )),
        })
        .collect()
}

fn assistant_to_user_blocks(blocks: Vec<AssistantContent>) -> Vec<UserContent> {
    blocks
        .into_iter()
        .map(|b| match b {
            AssistantContent::Text(t) | AssistantContent::Reasoning(t) => UserContent::Text(t),
            AssistantContent::ToolCall(c) => {
                UserContent::Text(format!("[tool-call {}({})]", c.name.as_str(), c.input))
            }
        })
        .collect()
}

/// FK on `sessions.participant_a_agent_id` / `participant_b_agent_id` rejects
/// unknown agent ids with Postgres `23503`. Map back to the typed
/// `AgentNotFound` so handlers can return a 400 instead of a 500.
fn map_agent_fk(e: sqlx::Error) -> SessionError {
    if let sqlx::Error::Database(ref db) = e
        && db.code().as_deref() == Some("23503")
    {
        // We don't know which side mismatched; surface with a sentinel
        // agent id (Nil UUID) so callers see the typed error and can
        // retry with a valid agent.
        return SessionError::AgentNotFound(AgentId::from(uuid::Uuid::nil()));
    }
    e.into()
}

fn invariant_to_backend(e: ParticipantDecodeError) -> SessionError {
    SessionError::Backend(format!("schema invariant: {e}"))
}

/// Low-cardinality label for the `patom.message.kind` span attribute.
fn chat_message_kind(message: &ChatMessage) -> &'static str {
    match message {
        ChatMessage::User(_) => "user",
        ChatMessage::Assistant(_) => "assistant",
    }
}

/// Number of content blocks in a [`ChatMessage`]. Cheap fan-out indicator
/// for the `session.append` span.
fn chat_message_block_count(message: &ChatMessage) -> usize {
    match message {
        ChatMessage::User(b) => b.len(),
        ChatMessage::Assistant(b) => b.len(),
    }
}
