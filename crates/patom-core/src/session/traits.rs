//! Session storage trait + opaque [`SessionId`].
//!
//! Sessions are 2-party (`Human ↔ Agent` or `Agent ↔ Agent`); a single
//! conversation between two parties. The agent never holds a `Vec<ChatMessage>`
//! directly — it asks a [`SessionStore`] for a viewer-scoped snapshot before
//! each turn and appends new messages back.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::{Caller, OrgId, UserId};
use crate::provider::ChatMessage;
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::SessionError;

crate::uuid_newtype! {
    /// Opaque session identifier. Constructed by the store, opaque to callers.
    pub SessionId
}

/// Tenancy projection of a session row — the columns the worker pool
/// needs to set `app.user_id` and the columns the `send_message` tool
/// needs to pin a child session to its parent's org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTenancy {
    pub org_id: OrgId,
    pub created_by_user_id: UserId,
}

/// Storage trait for conversation history. Implementations must be thread-safe.
///
/// Returns owned values rather than borrows — every backend (Postgres, Redis,
/// future S3) needs to allocate anyway and the caller (the agent) consumes
/// each result.
#[async_trait]
pub trait SessionStore: fmt::Debug + Send + Sync {
    /// Resolve or create the session bound to a canonical participant pair
    /// inside a DAG. Idempotent: two callers with the same `(root_request_id,
    /// canonical(a, b))` always reach the same session id. The store itself
    /// canonicalises the pair so a caller cannot accidentally insert the
    /// reversed-order row.
    ///
    /// `parent_session_id` is the session this conversation forks from (the
    /// caller's own session — `None` for the root human-to-agent session).
    ///
    /// `org_id` is the owning organisation; `created_by_user_id` is the
    /// human at the DAG root. Both are required because the underlying
    /// `sessions` row is `NOT NULL` on these columns and a trigger pins
    /// every child session to its parent's org so cross-tenant forks are
    /// rejected at the database boundary (see
    /// `migrations/00000000000016_sessions_tenancy.up.sql`).
    async fn resolve_or_create_for_pair(
        &self,
        root_request_id: PromptRequestId,
        a: Participant,
        b: Participant,
        parent_session_id: Option<SessionId>,
        org_id: OrgId,
        created_by_user_id: UserId,
    ) -> Result<SessionId, SessionError>;

    /// Tenant-scoped variant of [`Self::resolve_or_create_for_pair`].
    ///
    /// Opens a `begin_as_user(caller.user_id)` transaction so the row's
    /// RLS predicate is evaluated against the acting principal —
    /// cross-tenant forks fail at the WITH CHECK boundary. Used from
    /// worker / tool paths where the acting user is read off the
    /// claim's `created_by_user_id`; HTTP route paths keep the
    /// existing privileged entry point because they have already
    /// gated through `begin_as` upstream.
    ///
    /// The new session inherits `(org_id, created_by_user_id)` from
    /// `caller` — both `caller.org_id` and `caller.user_id` land on the
    /// row. They cannot diverge by construction (see [`Caller`]).
    async fn resolve_or_create_for_pair_for_user(
        &self,
        caller: &Caller,
        root_request_id: PromptRequestId,
        a: Participant,
        b: Participant,
        parent_session_id: Option<SessionId>,
    ) -> Result<SessionId, SessionError>;

    /// Append one chat message authored by `sender` and addressed to `receiver`.
    /// `sender == receiver` is rejected (self-messages are representationally
    /// invalid). The store does not check that `sender`/`receiver` are members
    /// of the session — the type-system invariant on `send_message` enforces
    /// that at the call site.
    ///
    /// `request_id` is the prompt request that produced this row, persisted on
    /// `session_messages.request_id` so downstream readers (the FE thread
    /// panel) can join history rows to live-stream bubbles by identity. The
    /// type-system invariant is that every production appender has a
    /// request_id in scope (turn execution, prompt append, send_message
    /// delivery, ping-pong nudges).
    async fn append(
        &self,
        id: SessionId,
        sender: MessageSender,
        receiver: Participant,
        message: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError>;

    /// Tenant-scoped variant of [`Self::append`]. Opens a
    /// `begin_as_user(acting_user_id)` transaction so the
    /// `session_messages` INSERT is gated by the RLS WITH CHECK on
    /// `org_id`; a worker or tool acting on behalf of a foreign-org
    /// user is rejected at the database boundary. The session's
    /// `org_id` is derived inside the same statement from the parent
    /// row so the column stays self-consistent.
    async fn append_for_user(
        &self,
        acting_user_id: UserId,
        id: SessionId,
        sender: MessageSender,
        receiver: Participant,
        message: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError>;

    /// Append a worker-injected `system`-kind nudge addressed to `receiver`.
    /// The body is wrapped as `ChatMessage::User(vec![Text(note)])` on the
    /// way in so the receiving agent renders it as user-side context.
    /// `request_id` follows the same contract as [`Self::append`].
    async fn append_system_nudge(
        &self,
        id: SessionId,
        receiver: Participant,
        note: String,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError>;

    /// Tenant-scoped variant of [`Self::append_system_nudge`]. Same
    /// RLS gate as [`Self::append_for_user`].
    async fn append_system_nudge_for_user(
        &self,
        acting_user_id: UserId,
        id: SessionId,
        receiver: Participant,
        note: String,
        request_id: PromptRequestId,
    ) -> Result<(), SessionError>;

    /// Render every message in `id` from `viewer`'s perspective:
    /// `sender == viewer` becomes `ChatMessage::Assistant`; everything else
    /// (including `system` rows) becomes `ChatMessage::User`. Used by
    /// `Agent::send_one_turn` to build the prompt without rebuilding history.
    async fn snapshot(
        &self,
        id: SessionId,
        viewer: Participant,
    ) -> Result<Vec<ChatMessage>, SessionError>;

    /// Paginated viewer-mapped snapshot. Returns up to `limit` messages
    /// ordered by `seq` ascending, taken from the most-recent end of the
    /// session — i.e. the rows with the largest `seq < before_seq` (or, when
    /// `before_seq` is `None`, the largest `seq`). Used by the `get_session`
    /// tool so the calling agent can pull a sibling's history without
    /// streaming the entire log.
    ///
    /// Each entry's `i64` is the row's `seq`; the smallest one is the next
    /// `before_seq` cursor for further pagination.
    async fn snapshot_window(
        &self,
        id: SessionId,
        viewer: Participant,
        limit: u32,
        before_seq: Option<i64>,
    ) -> Result<Vec<(i64, ChatMessage)>, SessionError>;

    /// Two participants of `id`. The pair is in canonical order so callers
    /// can pattern-match deterministically.
    async fn participants(&self, id: SessionId)
    -> Result<(Participant, Participant), SessionError>;

    /// `Some(parent)` when `id` was forked from another session by a
    /// `send_message` call; `None` for the human-to-agent root.
    async fn parent(&self, id: SessionId) -> Result<Option<SessionId>, SessionError>;

    /// Single-RTT: viewer-mapped history of `id`'s parent — but *only* when
    /// `viewer` is itself a participant of the parent. Otherwise (no parent;
    /// viewer not in parent's participants; parent empty) returns an empty
    /// vector. Called on every turn the agent loop runs; the default impl
    /// fans out to `parent` + `participants` + `snapshot` (three round-trips)
    /// so the optimization is opt-in per backend. The Postgres impl
    /// overrides with one query that joins through `sessions` to
    /// `session_messages`.
    async fn parent_history_for_viewer(
        &self,
        id: SessionId,
        viewer: Participant,
    ) -> Result<Vec<ChatMessage>, SessionError> {
        let Some(parent) = self.parent(id).await? else {
            return Ok(Vec::new());
        };
        let (a, b) = self.participants(parent).await?;
        if viewer != a && viewer != b {
            return Ok(Vec::new());
        }
        self.snapshot(parent, viewer).await
    }

    /// DAG anchor — the `prompt_requests.id` that rooted the conversation
    /// tree this session belongs to. Used by the `send_message` tool to
    /// resolve sibling sessions and by the DAG-budget bump.
    async fn root_request_id(&self, id: SessionId) -> Result<PromptRequestId, SessionError>;

    /// Tenancy lookup — `(org_id, created_by_user_id)` for the row.
    /// Used by callers that derive a child session's tenancy from the
    /// parent (the `send_message` tool, the runtime queue's
    /// implicit-create path on enqueue) and by the worker pool to set
    /// `app.user_id` from the claimed session.
    async fn tenancy(&self, id: SessionId) -> Result<SessionTenancy, SessionError>;

    /// Drop the session and every message it owns.
    async fn delete(&self, id: SessionId) -> Result<(), SessionError>;
}

/// Cheap-clone handle so consumers can hold the store without a generic parameter.
pub type SharedSessionStore = Arc<dyn SessionStore>;
