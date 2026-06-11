//! Thread-feed storage trait + opaque ids.
//!
//! A thread is a flat, multi-participant chat feed (Slack-exact: channel
//! timeline + threaded replies). An agent's "session" is its participation in a
//! thread — `(thread_id, agent_id)`, identified by an [`AgentThreadId`]
//! (`agent_thread_state.id`). Everyone else in the thread is its counterparty.
//!
//! The feed is one ordered log per thread (`thread_messages`, ordered by `seq`).
//! `kind` discriminates **posted** chat (visible to, and ingested by, everyone)
//! from per-agent **private artifacts** (reasoning / tool_use / tool_result /
//! system_note) that are displayed to all but ingested only by their owner.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::auth::{Caller, UserId};
use crate::channels::ChannelId;
use crate::colleagues::{ColleagueId, ColleagueName};
use crate::provider::ChatMessage;
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::ThreadError;

/// A thread's listing row — the membership-scoped feed index (P7).
///
/// Carries the fields a Slack-style timeline needs without a per-thread G2
/// round-trip: the root posted message's summary + how many posted replies
/// hang under it. The full feed is still read via the HTTP feed read.
#[derive(Debug, Clone)]
pub struct ThreadListItem {
    pub thread_id: ThreadId,
    pub channel_id: Option<ChannelId>,
    pub last_activity_at: DateTime<Utc>,
    /// First `posted` row of the thread — the timeline message this thread
    /// renders as. `None` for a thread with no posted rows yet (e.g. a
    /// scheduled seed whose agent hasn't replied).
    pub root: Option<RootSummary>,
    /// Posted rows beyond the root (Slack's "N replies"). Never negative.
    pub reply_count: i64,
}

/// Summary of a thread's root posted message for the timeline view.
#[derive(Debug, Clone)]
pub struct RootSummary {
    /// First text block of the root message, capped at
    /// [`super::limits::ROOT_SNIPPET_MAX_CHARS`] in SQL (CLAUDE.md §5).
    pub snippet: String,
    pub sender: MessageSender,
    pub created_at: DateTime<Utc>,
}

/// Which feed `list_threads` reads (CLAUDE.md §1: a sum, not bool + Option).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadScope {
    /// One channel's threads, gated on the caller's membership.
    Channel(ChannelId),
    /// The caller's direct messages. `counterpart = Some(c)` narrows to the
    /// conversation between the caller and colleague `c` (both orientations:
    /// threads the caller started with `c`, and threads `c` started with the
    /// caller). `None` = every DM the caller can see.
    Dms { counterpart: Option<ColleagueId> },
}

/// One row of the canonical flat thread feed (the G2 read).
///
/// Unlike [`ThreadStore::context_for_agent`] (an agent's viewer-mapped *LLM*
/// context), this is the display feed: every row in `seq` order with its `kind`
/// exposed (posted chat ∪ everyone's reasoning / tool_use / tool_result /
/// system_note — agent thinking is shown to all for transparency, §2). The FE
/// renders private artifacts differently from posted chat. Both participant
/// sides are decoded once in the store via the canonical `Participant::try_from`
/// (§1): `sender` is [`MessageSender::System`] on a System row; `receiver` is
/// `None` when the row addresses no one. `owner_agent_id` is set on every
/// non-`Posted` row.
#[derive(Debug, Clone)]
pub struct FeedMessage {
    pub seq: i64,
    pub kind: MessageKind,
    pub sender: MessageSender,
    pub owner_agent_id: Option<AgentId>,
    pub receiver: Option<Participant>,
    pub body: serde_json::Value,
    pub request_id: Option<PromptRequestId>,
    /// Client dedupe key the posting submit carried (`NewMessage::
    /// idempotency_key`). The FE reconciles its optimistic bubble against the
    /// persisted echo by this key; `None` for agent-produced rows.
    pub client_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

crate::uuid_newtype! {
    /// Opaque thread identifier (`threads.id`).
    pub ThreadId
}

crate::uuid_newtype! {
    /// Opaque agent-participation identifier (`agent_thread_state.id`) — an
    /// agent's "session" in a thread. Doubles as the queue's `claim_key` for
    /// chat turns.
    pub AgentThreadId
}

crate::uuid_newtype! {
    /// Opaque surface id of a feed row (`thread_messages.id`). Lets a channel
    /// message root a reply-thread that lives under a different `thread_id`.
    pub ThreadMessageId
}

crate::str_enum! {
    /// Feed row kind. `posted` rows are everyone's chat; the rest are one
    /// agent's private artifacts (owner-scoped). Single source of truth for the
    /// `thread_messages.kind` CHECK + JSON wire.
    pub enum MessageKind {
        Posted     => "posted",
        Reasoning  => "reasoning",
        ToolUse    => "tool_use",
        ToolResult => "tool_result",
        SystemNote => "system_note",
    }
}

/// A row to append to a thread feed.
///
/// `sender = None` encodes the synthetic
/// System sender (tool results, nudges, scheduled seeds). `owner_agent_id` is
/// required for every non-`Posted` kind and forbidden for `Posted` (enforced by
/// the `thread_messages_owner_kind` CHECK). `receiver` may be set only on a
/// `Posted` row (an @-addressed message).
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub kind: MessageKind,
    pub sender: Option<ColleagueId>,
    pub owner_agent_id: Option<AgentId>,
    pub receiver: Option<ColleagueId>,
    pub body: ChatMessage,
    pub request_id: Option<PromptRequestId>,
    /// Client-supplied dedupe key for human-posted rows (`POST /prompts`).
    /// Unique per org when set; an append that collides returns the existing
    /// row's id instead of inserting a duplicate — an untagged post creates no
    /// trigger row, so this is its only retry guard. `None` for every
    /// agent-produced row.
    pub idempotency_key: Option<crate::runtime::IdempotencyKey>,
}

/// Storage trait for the thread feed. Implementations must be thread-safe.
#[async_trait]
pub trait ThreadStore: fmt::Debug + Send + Sync {
    /// Create a thread. `channel_id = None` => a DM thread, which REQUIRES
    /// `dm_counterpart` (the colleague — human or agent — the conversation is
    /// with; both it and the creator can see the thread). A channel thread
    /// must pass `dm_counterpart = None` (DB CHECK forbids both).
    /// `root_message_id` is the channel-timeline message this reply-thread
    /// hangs under (None for the channel's own timeline thread + DMs).
    /// Tenant-scoped: the row's `org_id` comes from `caller` and is gated by
    /// the RLS WITH CHECK.
    ///
    /// The counterpart bounds *human visibility only* — any agent can still be
    /// invoked into the thread (agents are org-global).
    async fn create_thread(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
        root_message_id: Option<ThreadMessageId>,
        created_by: ColleagueId,
        dm_counterpart: Option<ColleagueId>,
    ) -> Result<ThreadId, ThreadError>;

    /// Resolve (or create) `agent`'s participation in `thread`. Idempotent on
    /// `(thread, agent)`. Returns the participation id — the chat `claim_key`.
    async fn resolve_participation(
        &self,
        caller: &Caller,
        thread: ThreadId,
        agent: AgentId,
    ) -> Result<AgentThreadId, ThreadError>;

    /// Append one feed row, allocating the next per-thread `seq` atomically and
    /// bumping `threads.last_activity_at`. Returns the new row's
    /// [`ThreadMessageId`] — the surface id a reply-thread roots on and the
    /// `trigger_message_id` an agent→agent wake carries.
    async fn append(
        &self,
        caller: &Caller,
        thread: ThreadId,
        message: NewMessage,
    ) -> Result<ThreadMessageId, ThreadError>;

    /// List the threads `caller` may see, scoped by **membership**, not by who
    /// created them (P7). [`ThreadScope::Channel`] ⇒ that channel's threads,
    /// gated on the caller being a `channel_members` row and the channel not
    /// archived. [`ThreadScope::Dms`] ⇒ the caller's DMs — threads the caller
    /// created *or* is the counterpart of. Org-pinned (`caller.org_id`) so a
    /// multi-org member's other workspaces never leak in (RLS gates
    /// membership, not the active org). Ordered newest-activity first.
    async fn list_threads(
        &self,
        caller: &Caller,
        scope: ThreadScope,
    ) -> Result<Vec<ThreadListItem>, ThreadError>;

    /// Whether `user_id` may receive a posted message in `thread` — the
    /// `send_message` human gate (no auto-add).
    ///
    /// A channel thread requires the human to be in `channel_members`; a DM
    /// thread (`channel_id IS NULL`) is reachable only by its pair — the
    /// creator or the counterpart. Returns [`ThreadError::NotFound`] if the
    /// thread is missing. Privileged read — agents are org-global and gate
    /// humans by membership, not by the acting principal.
    async fn is_channel_member(
        &self,
        thread: ThreadId,
        user_id: UserId,
    ) -> Result<bool, ThreadError>;

    /// The DM counterpart colleague of `thread`, or `None` for a channel
    /// thread / a legacy DM with no counterpart. Returns
    /// [`ThreadError::NotFound`] if the thread is missing. Privileged point
    /// lookup — `POST /prompts` routes an untagged DM message to the
    /// counterpart agent, and that read happens before any append.
    async fn dm_counterpart(&self, thread: ThreadId) -> Result<Option<ColleagueId>, ThreadError>;

    /// The canonical flat feed for `thread` in `seq` order — the G2 read. Every
    /// row (posted chat ∪ everyone's private artifacts) with its `kind` exposed
    /// and both participant sides resolved to colleague satellites for the HTTP
    /// boundary. Runs RLS-scoped under `caller` so a thread the caller can't see
    /// (cross-org, or a channel they're not in) yields an empty page rather than
    /// a leak. `before_seq = Some` pages backwards (rows with `seq < before_seq`);
    /// `LIMIT MAX_THREAD_FEED`.
    async fn feed(
        &self,
        caller: &Caller,
        thread: ThreadId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<FeedMessage>, ThreadError>;

    /// The channel a thread belongs to, or `None` for a DM thread. Returns
    /// [`ThreadError::NotFound`] if the thread is missing. Privileged read —
    /// callers (the `schedule_task` tool) need the location of a thread they
    /// are already participating in to inherit it onto a scheduled task.
    async fn channel_of(&self, thread: ThreadId) -> Result<Option<ChannelId>, ThreadError>;

    /// Advance the reflection checkpoint for `(agent, thread)` to
    /// `up_to_message_id` after a successful background reflection. Upsert on
    /// the `(agent_id, thread_id)` PK so the next reflection picks up strictly
    /// after this message; without it the scheduler re-enqueues the same idle
    /// window every tick. Privileged write — the worker's background path holds
    /// no per-request principal (reflection is org-global cognition).
    async fn advance_reflection_checkpoint(
        &self,
        org_id: crate::auth::OrgId,
        agent: AgentId,
        thread: ThreadId,
        up_to_message_id: ThreadMessageId,
    ) -> Result<(), ThreadError>;

    /// The most-recently-joined agent participating in `thread`, or `None` if
    /// no agent has participation yet. Privileged point lookup used by the Slack
    /// inbound bridge to route a plain (un-@-tagged) thread reply to the agent
    /// the conversation is with — a Slack thread maps to one Patom thread.
    async fn last_agent(&self, thread: ThreadId) -> Result<Option<AgentId>, ThreadError>;

    /// Whether `caller` may see `thread` — the SSE-subscription gate (G3). Same
    /// member-or-DM-owner + not-archived + active-org-pin predicate as `feed` /
    /// `list_threads`, run RLS-scoped under the caller. Returns `false` (not an
    /// error) for a thread that is cross-org, in a channel the caller isn't a
    /// member of, or simply missing — so the route 404s without leaking
    /// existence.
    async fn visible_to(&self, caller: &Caller, thread: ThreadId) -> Result<bool, ThreadError>;

    /// Build `agent`'s LLM context for `thread`: every `posted` row (from
    /// anyone) plus `agent`'s own private artifacts, in `seq` order, mapped to
    /// `viewer`'s perspective (own utterances → Assistant, others → User).
    /// Peers' private rows are excluded. Privileged read — an agent is
    /// org-global within its org.
    ///
    /// Each non-viewer `posted` message is prefixed with its sender's name so
    /// the agent can tell speakers apart in a multi-party thread. `overrides`
    /// supplies per-platform labels (e.g. Slack handles) keyed by colleague
    /// id; senders absent from it fall back to their canonical name.
    async fn context_for_agent(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
        overrides: &HashMap<ColleagueId, ColleagueName>,
    ) -> Result<Vec<ChatMessage>, ThreadError>;
}

/// Cheap-clone handle so consumers hold the store without a generic parameter.
pub type SharedThreadStore = Arc<dyn ThreadStore>;
