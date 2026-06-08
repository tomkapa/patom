//! Postgres-backed [`SessionStore`].
//!
//! Conversation history is stored in `session_messages (session_id, seq,
//! sender_colleague_id, receiver_colleague_id, body JSONB, created_at)`. The
//! body column carries the full [`ChatMessage`] envelope as JSONB so adding a
//! content variant is a code change, not a migration. Per-session ordering is
//! provided by the `seq` column, assigned monotonically inside `append`.
//!
//! `sender_colleague_id IS NULL` encodes the synthetic `System` sender
//! (worker-injected nudges). Receivers are never System — the column is NOT
//! NULL. Sessions track both ends as `participant_a_colleague_id NOT NULL` +
//! `participant_b_colleague_id NULL` (NULL = System counterpart of a
//! reflection/resolution session).
//!
//! Decoding a [`MessageSender`] / [`Participant`] from a row requires the
//! satellite (`ColleagueKind` plus `user_id`/`agent_id`). We `LEFT JOIN` the
//! `colleagues` table on every participant-bearing read so the full shape
//! reconstructs in one round-trip — same model as
//! [`crate::colleagues::PgColleagueStore::list_for_org`].
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
use crate::colleagues::{ColleagueId, ColleagueKind};
use crate::provider::{AssistantContent, ChatMessage, UserContent};
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::SessionError;
use super::limits::MAX_MESSAGES_PER_SESSION;
use super::traits::{SessionId, SessionStore, SessionTenancy};

// ─── Decoded-row helpers ────────────────────────────────────────────────────
//
// Every SELECT that hydrates a participant joins through `colleagues` so the
// satellite columns (`kind`, `user_id`, `agent_id`) land in the same query.
// The decode helpers below promote `(colleague_id_opt, kind_opt, user_id_opt,
// agent_id_opt)` tuples back to the typed enums, surfacing schema violations
// as `SessionError::Backend` per §6.

/// Per-side join SELECT columns. `alias` is the join alias for the colleague
/// row. Used via `concat!` to keep SQL constants `&'static str`.
macro_rules! participant_cols {
    ($alias:literal) => {
        concat!(
            $alias,
            ".id, ",
            $alias,
            ".kind, ",
            $alias,
            ".user_id, ",
            $alias,
            ".agent_id"
        )
    };
}

type ParticipantTuple = (
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
);

/// Decode a participant tuple (the four joined satellite columns) back to the
/// typed [`Participant`], mapping a schema-shape violation to [`SessionError`].
/// `MessageSender` callers convert via [`MessageSender::from_participant`] —
/// both share the same shape, owned by [`Participant::from_colleague_columns`].
fn decode_participant(tuple: ParticipantTuple) -> Result<Participant, SessionError> {
    Participant::from_colleague_columns(tuple)
        .map_err(|reason| SessionError::Backend(format!("schema invariant: {reason}")))
}

fn decode_sender(tuple: ParticipantTuple) -> Result<MessageSender, SessionError> {
    decode_participant(tuple).map(MessageSender::from_participant)
}

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

// ─── SQL constants ──────────────────────────────────────────────────────────

/// Snapshot path: every message in a session, ordered by `seq`. The sender
/// join is `LEFT` because `sender_colleague_id IS NULL` encodes the
/// worker-injected System sender; receiver join is inner because the
/// receiver is NOT NULL on `session_messages`.
const SNAPSHOT_SQL: &str = concat!(
    "SELECT ",
    concat!(
        participant_cols!("sc"),
        ", ",
        participant_cols!("rc"),
        ", m.body"
    ),
    " FROM session_messages m \
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
     JOIN colleagues rc ON rc.id = m.receiver_colleague_id \
     WHERE m.session_id = $1 \
     ORDER BY m.seq ASC"
);

const SNAPSHOT_WINDOW_SQL: &str = concat!(
    "SELECT m.seq, ",
    participant_cols!("sc"),
    ", ",
    participant_cols!("rc"),
    ", m.body \
     FROM session_messages m \
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id \
     JOIN colleagues rc ON rc.id = m.receiver_colleague_id \
     WHERE m.session_id = $1 AND m.seq < $2 \
     ORDER BY m.seq DESC \
     LIMIT $3"
);

/// Pull every parent-session message for a given child session, scoped to
/// rows whose parent includes the `viewer` colleague. The viewer is bound as
/// `$2`; a real-colleague viewer matches either slot of the parent session,
/// a System viewer cannot (its `colleague_id()` is None — guarded at the
/// boundary). The predicate uses `=` (not `IS NOT DISTINCT FROM`) because
/// `participant_a_colleague_id` is NOT NULL and a NULL slot `b` legitimately
/// shouldn't match anything.
const PARENT_HISTORY_SQL: &str = concat!(
    "WITH parent_session AS (
         SELECT s.id AS parent_id
         FROM sessions cur
         JOIN sessions s ON s.id = cur.parent_session_id
         WHERE cur.id = $1
           AND (s.participant_a_colleague_id = $2
             OR s.participant_b_colleague_id = $2)
     )
     SELECT ",
    participant_cols!("sc"),
    ", ",
    participant_cols!("rc"),
    ", m.body
     FROM session_messages m
     LEFT JOIN colleagues sc ON sc.id = m.sender_colleague_id
     JOIN colleagues rc ON rc.id = m.receiver_colleague_id
     JOIN parent_session ps ON m.session_id = ps.parent_id
     ORDER BY m.seq ASC"
);

const PARTICIPANTS_SQL: &str = concat!(
    "SELECT ",
    participant_cols!("ac"),
    ", ",
    participant_cols!("bc"),
    " FROM sessions s
     LEFT JOIN colleagues ac ON ac.id = s.participant_a_colleague_id
     LEFT JOIN colleagues bc ON bc.id = s.participant_b_colleague_id
     WHERE s.id = $1"
);

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
        viewer: ColleagueId,
    ) -> Result<Vec<ChatMessage>, SessionError> {
        let rows: Vec<MessageRow> =
            run_privileged::<Option<Vec<MessageRow>>, SessionError>(&self.pool, async |tx| {
                let exists: Option<(SessionId,)> =
                    sqlx::query_as("SELECT id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?;
                if exists.is_none() {
                    return Ok(None);
                }
                let rows: Vec<MessageRow> = sqlx::query_as(SNAPSHOT_SQL)
                    .bind(id)
                    .fetch_all(&mut **tx)
                    .await?;
                Ok(Some(rows))
            })
            .await?
            .ok_or(SessionError::NotFound(id))?;
        tracing::Span::current().record("patom.history.count", rows.len());

        let mut out = Vec::with_capacity(rows.len());
        for raw in rows {
            let (sender_tuple, receiver_tuple, body) = split_message_row(raw);
            let sender = decode_sender(sender_tuple)?;
            let receiver = decode_participant(receiver_tuple)?;
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
        viewer: ColleagueId,
        limit: u32,
        before_seq: Option<i64>,
    ) -> Result<Vec<(i64, ChatMessage)>, SessionError> {
        let limit_i64 = i64::from(limit);
        let cursor = before_seq.unwrap_or(i64::MAX);
        let mut rows =
            run_privileged::<Option<Vec<WindowMessageRow>>, SessionError>(&self.pool, async |tx| {
                let exists: Option<(SessionId,)> =
                    sqlx::query_as("SELECT id FROM sessions WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?;
                if exists.is_none() {
                    return Ok(None);
                }
                let rows: Vec<WindowMessageRow> = sqlx::query_as(SNAPSHOT_WINDOW_SQL)
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
        for (seq, raw) in rows.into_iter().map(split_window_row) {
            let (sender_tuple, receiver_tuple, body) = raw;
            let sender = decode_sender(sender_tuple)?;
            let receiver = decode_participant(receiver_tuple)?;
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
            // a side (NOT NULL)
            Option<ColleagueId>,
            Option<ColleagueKind>,
            Option<UserId>,
            Option<AgentId>,
            // b side (NULL = System)
            Option<ColleagueId>,
            Option<ColleagueKind>,
            Option<UserId>,
            Option<AgentId>,
        );
        let row = run_privileged::<Option<Row>, SessionError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(PARTICIPANTS_SQL)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;

        let (acid, akind, auid, aaid, bcid, bkind, buid, baid) =
            row.ok_or(SessionError::NotFound(id))?;
        let a = decode_participant((acid, akind, auid, aaid))?;
        let b = decode_participant((bcid, bkind, buid, baid))?;
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
    /// parent session, applies the viewer-colleague predicate inline, and the
    /// final SELECT joins through `session_messages`.
    ///
    /// The viewer must be a real colleague (Human or Agent); a System viewer
    /// has no `colleague_id` and the predicate would match nothing — guarded
    /// here as a backend invariant since worker turns only run for agent
    /// receivers (the doc on `crate::memory::AgentMemory::system_prompt`
    /// formalises this).
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
        viewer: ColleagueId,
    ) -> Result<Vec<ChatMessage>, SessionError> {
        let rows = run_privileged::<Vec<MessageRow>, SessionError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(PARENT_HISTORY_SQL)
                .bind(id)
                .bind(viewer)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;

        // The optimized join returns zero rows for three cases that the trait
        // default distinguishes: a nonexistent `id` (→ `NotFound`), a session
        // with no parent, and a viewer who is not a parent participant (both
        // → empty). Preserve the `NotFound` contract by probing existence only
        // on the empty path, so the common non-empty hot path stays one
        // round-trip (the whole point of this override).
        if rows.is_empty() {
            self.parent(id).await?;
        }

        tracing::Span::current().record("patom.history.count", rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for raw in rows {
            let (sender_tuple, receiver_tuple, body) = split_message_row(raw);
            let sender = decode_sender(sender_tuple)?;
            let receiver = decode_participant(receiver_tuple)?;
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

// ─── Row decode types ───────────────────────────────────────────────────────
//
// The wide tuples for sqlx::query_as rows. Each participant side is 4 cols:
// (colleague_id?, kind?, user_id?, agent_id?). System (NULL) decodes to all-None.

/// One `session_messages` row decoded with both sides' satellite columns.
type MessageRow = (
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    serde_json::Value,
);

/// Same shape as [`MessageRow`] but with the row's `seq` prepended.
type WindowMessageRow = (
    i64,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    Option<ColleagueId>,
    Option<ColleagueKind>,
    Option<UserId>,
    Option<AgentId>,
    serde_json::Value,
);

fn split_message_row(row: MessageRow) -> (ParticipantTuple, ParticipantTuple, serde_json::Value) {
    let (
        sender_colleague_id,
        sender_kind,
        sender_user_id,
        sender_agent_id,
        receiver_colleague_id,
        receiver_kind,
        receiver_user_id,
        receiver_agent_id,
        body,
    ) = row;
    (
        (
            sender_colleague_id,
            sender_kind,
            sender_user_id,
            sender_agent_id,
        ),
        (
            receiver_colleague_id,
            receiver_kind,
            receiver_user_id,
            receiver_agent_id,
        ),
        body,
    )
}

fn split_window_row(
    row: WindowMessageRow,
) -> (i64, (ParticipantTuple, ParticipantTuple, serde_json::Value)) {
    let (
        seq,
        sender_colleague_id,
        sender_kind,
        sender_user_id,
        sender_agent_id,
        receiver_colleague_id,
        receiver_kind,
        receiver_user_id,
        receiver_agent_id,
        body,
    ) = row;
    (
        seq,
        (
            (
                sender_colleague_id,
                sender_kind,
                sender_user_id,
                sender_agent_id,
            ),
            (
                receiver_colleague_id,
                receiver_kind,
                receiver_user_id,
                receiver_agent_id,
            ),
            body,
        ),
    )
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
    // §6: slot `a` is always a real colleague (System sorts last and ends up
    // in slot `b`); the schema's NOT NULL on `participant_a_colleague_id`
    // expresses the same invariant.
    let a_colleague = a.colleague_id().ok_or_else(|| {
        SessionError::Backend(
            "canonical pair returned System in slot a — invariant violation".to_string(),
        )
    })?;
    let b_colleague = b.colleague_id();
    let now = DateTime::<Utc>::from(store.clock.now_wall());
    let new_id = SessionId::new();

    let (id, inserted): (SessionId, bool) = sqlx::query_as(
        "INSERT INTO sessions
             (id, created_at, org_id, created_by_user_id,
              parent_session_id, root_request_id,
              participant_a_colleague_id, participant_b_colleague_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (org_id, root_request_id,
                      participant_a_colleague_id, participant_b_colleague_id)
             DO UPDATE SET id = sessions.id
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(new_id)
    .bind(now)
    .bind(org_id)
    .bind(created_by_user_id)
    .bind(parent_session_id)
    .bind(root_request_id)
    .bind(a_colleague)
    .bind(b_colleague)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_colleague_fk)?;

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
/// or success).
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
    let body = serde_json::to_value(&message)
        .map_err(|e| SessionError::Backend(format!("serialize message: {e}")))?;
    let cap = store.message_cap;
    let cap_i64 = i64::from(cap);

    // `sender_colleague_id` is nullable (NULL ⇒ System nudge). The receiver
    // must always be a real colleague — Participant::System as receiver is a
    // backend invariant violation.
    let sender_colleague = sender.colleague_id();
    let receiver_colleague = receiver.colleague_id().ok_or_else(|| {
        SessionError::Backend(
            "append_row called with System receiver — receivers are never System".to_string(),
        )
    })?;

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
                  sender_colleague_id, receiver_colleague_id,
                  body, created_at, request_id, org_id)
             SELECT $1, stats.next_seq, $3, $4, $5, $6, $7,
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
    .bind(sender_colleague)
    .bind(receiver_colleague)
    .bind(body)
    .bind(now)
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_colleague_fk)?;

    let Some((inserted, row_count)) = row else {
        // Outer SELECT had no rows ⇒ the `locked` CTE found no session row
        // (`FOR UPDATE` matched nothing).
        return Err(SessionError::NotFound(id));
    };
    if !inserted {
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
/// `sender.colleague_id() == Some(viewer)` ⇒ assistant; everything else ⇒
/// user. The stored `ChatMessage` already has its content split into
/// User/Assistant variants; we re-tag without altering the content blocks.
///
/// `receiver` is the row's addressee — needed to decide whether a
/// `ToolResult` block belongs to the viewer's own tool call or to the other
/// side's. In an agent↔agent session both parties' tool-result rows share
/// `sender == System`, so receiver is the only field that disambiguates them.
fn map_message_for_viewer(
    stored: ChatMessage,
    sender: MessageSender,
    receiver: Participant,
    viewer: ColleagueId,
) -> ChatMessage {
    // Self-detection: a System sender (no colleague_id) is never the viewer.
    let is_self = sender.colleague_id() == Some(viewer);

    match (is_self, stored) {
        (true, ChatMessage::Assistant(blocks)) => ChatMessage::Assistant(blocks),
        (true, ChatMessage::User(blocks)) => {
            ChatMessage::Assistant(user_to_assistant_blocks(blocks))
        }
        (false, ChatMessage::Assistant(blocks)) => {
            ChatMessage::User(assistant_to_user_blocks(blocks))
        }
        (false, ChatMessage::User(blocks)) => {
            ChatMessage::User(retag_other_side_user_blocks(blocks, receiver, viewer))
        }
    }
}

/// On a `(false, User)` row, fold any `ToolResult` whose matching tool call
/// did not survive viewer-mapping into a marker `Text` block.
fn retag_other_side_user_blocks(
    blocks: Vec<UserContent>,
    receiver: Participant,
    viewer: ColleagueId,
) -> Vec<UserContent> {
    // The tool-result belongs to the viewer's own tool call iff the row's
    // receiver is the viewer — same colleague_id check as the self detection.
    if receiver.colleague_id() == Some(viewer) {
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

/// FK on `sessions.participant_*_colleague_id` / `session_messages.*` rejects
/// unknown colleague ids with Postgres `23503`. Map back to the typed
/// `ColleagueNotFound` so handlers can return a 400 instead of a 500.
fn map_colleague_fk(e: sqlx::Error) -> SessionError {
    if let sqlx::Error::Database(ref db) = e
        && db.code().as_deref() == Some("23503")
        && db.constraint().is_some_and(|c| c.contains("colleague"))
    {
        // A colleague-FK violation specifically — the constraint name carries
        // the `*_colleague_id_fkey` column. We don't know which side mismatched;
        // surface with a sentinel colleague id (Nil UUID) so callers see the
        // typed error. Any *other* `23503` falls through to the generic `Db`
        // error rather than being mislabelled as an unknown colleague.
        return SessionError::ColleagueNotFound(ColleagueId::from(uuid::Uuid::nil()));
    }
    e.into()
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
