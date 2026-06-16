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

/// A `thread_messages.seq` value — the per-thread monotonic feed ordinal.
///
/// A newtype (CLAUDE.md §1) so a feed position can't be confused with any other
/// `i64`. Used by the compaction read path: `context_tail` returns rows with
/// `seq > since`, and a compaction records the highest `seq` it folded in as
/// `covers_through_seq`. The underlying column is `BIGINT NOT NULL` and the
/// generator starts at 1, so the value is always non-negative — the smart
/// constructor enforces that. The legacy `feed(before_seq: Option<i64>)` cursor
/// keeps its bare `i64` (a pure pagination bound, no invariant to protect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(i64);

impl Seq {
    /// The pre-feed origin — `seq > Seq::ZERO` selects the whole thread. Used as
    /// the `since` bound when an agent has no compaction yet.
    pub const ZERO: Self = Self(0);

    /// The raw ordinal, for binding into SQL.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for Seq {
    type Error = crate::types::ParseError;
    fn try_from(raw: i64) -> Result<Self, Self::Error> {
        if raw < 0 {
            return Err(crate::types::ParseError::OutOfRange {
                field: "seq",
                detail: "must be non-negative",
            });
        }
        Ok(Self(raw))
    }
}

/// One row of an agent's windowed context: a chat message tagged with its [`Seq`].
///
/// The seq lets the compaction layer record how far into the feed an overflow cut
/// reached (`covers_through_seq`) without re-querying.
#[derive(Debug, Clone)]
pub struct TailRow {
    pub seq: Seq,
    pub message: ChatMessage,
}

/// The bounded, perspective-mapped, tool-pair-repaired slice of a thread an
/// agent reads on one turn — the output of [`ThreadStore::context_tail`].
///
/// Rows are oldest-first and capped at `MAX_CONTEXT_MESSAGES` (the windowing
/// floor). `since`-filtered: only rows with `seq > since` are present, so a
/// caller that already folded everything up to `since` into a summary sees only
/// what is new.
#[derive(Debug, Clone, Default)]
pub struct ContextTail {
    pub rows: Vec<TailRow>,
}

impl ContextTail {
    /// Number of rows in the window.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the window is empty (no feed rows past `since`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Highest feed `seq` present in the window, if any.
    #[must_use]
    pub fn max_seq(&self) -> Option<Seq> {
        self.rows.iter().map(|r| r.seq).max()
    }

    /// Drop the seqs, yielding just the messages in order (the request payload).
    #[must_use]
    pub fn into_messages(self) -> Vec<ChatMessage> {
        self.rows.into_iter().map(|r| r.message).collect()
    }
}

/// A loaded `thread_compactions` row — the rolling summary plus cooldown state.
///
/// `summary` is the raw stored text; the `agent_core` compaction layer wraps it in
/// a `CompactionSummary` (which enforces the size cap) when it needs the typed
/// invariant. Storage stays provider-free.
#[derive(Debug, Clone)]
pub struct ThreadCompaction {
    pub summary: String,
    /// Highest feed `seq` folded into `summary`; the read path passes this as
    /// `since` so only newer rows are returned verbatim.
    pub covers_through_seq: Seq,
    pub summary_tokens: i32,
    /// Consecutive summarizer failures; 0 after a success.
    pub failed_attempts: i32,
    /// While `now < cooldown_until` the caller skips the summarizer and serves
    /// the floor + this (possibly stale) summary. `None` = no active cooldown.
    pub cooldown_until: Option<DateTime<Utc>>,
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
    ///
    /// Thin wrapper over [`context_tail`](ThreadStore::context_tail) with
    /// `since = Seq::ZERO` (the whole thread, still bounded by the windowing
    /// floor). Kept for callers that don't compact.
    async fn context_for_agent(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
        overrides: &HashMap<ColleagueId, ColleagueName>,
    ) -> Result<Vec<ChatMessage>, ThreadError> {
        Ok(self
            .context_tail(thread, agent, viewer, Seq::ZERO, overrides)
            .await?
            .into_messages())
    }

    /// Build `agent`'s bounded LLM context for `thread`, returning only rows with
    /// `seq > since` (everything older is assumed folded into a compaction
    /// summary the caller holds). Same perspective-map + tool-pair repair as
    /// [`context_for_agent`](ThreadStore::context_for_agent), plus:
    ///  - a hard `LIMIT MAX_CONTEXT_MESSAGES` (the windowing floor — the prompt
    ///    is bounded even with no summary and no LLM);
    ///  - oversized `tool_result` bodies are render-capped to
    ///    `MAX_TOOL_RESULT_CHARS` (the underlying feed row is never mutated);
    ///  - each kept row carries its `seq` so the compaction layer can record
    ///    `covers_through_seq` after a cut.
    ///
    /// Privileged read — an agent is org-global within its org.
    async fn context_tail(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
        since: Seq,
        overrides: &HashMap<ColleagueId, ColleagueName>,
    ) -> Result<ContextTail, ThreadError>;

    /// Load the rolling compaction for `(thread, agent)`, or `None` if the agent
    /// has never compacted this thread. Privileged point lookup (the PK is a
    /// pair of globally-unique ids, so no cross-org leak).
    async fn load_compaction(
        &self,
        thread: ThreadId,
        agent: AgentId,
    ) -> Result<Option<ThreadCompaction>, ThreadError>;

    /// Upsert a successful compaction: replace the summary, advance
    /// `covers_through_seq`, and clear the failure cooldown (`failed_attempts=0`,
    /// `cooldown_until=NULL`). Privileged write — compaction is org-global agent
    /// cognition; `org` stamps the row for RLS.
    async fn save_compaction(
        &self,
        org: crate::auth::OrgId,
        thread: ThreadId,
        agent: AgentId,
        summary: &str,
        covers_through_seq: Seq,
        summary_tokens: i32,
    ) -> Result<(), ThreadError>;

    /// Record a summarizer failure for `(thread, agent)`: increment
    /// `failed_attempts` and set `cooldown_until` so the next turns skip the LLM
    /// and serve the windowing floor. Inserts a minimal (empty-summary) row when
    /// the agent had no prior compaction. Privileged write.
    async fn bump_cooldown(
        &self,
        org: crate::auth::OrgId,
        thread: ThreadId,
        agent: AgentId,
        cooldown_until: DateTime<Utc>,
    ) -> Result<(), ThreadError>;
}

/// Cheap-clone handle so consumers hold the store without a generic parameter.
pub type SharedThreadStore = Arc<dyn ThreadStore>;

#[cfg(test)]
mod seq_tests {
    use super::Seq;

    #[test]
    fn zero_is_the_origin() {
        assert_eq!(Seq::ZERO.get(), 0);
    }

    #[test]
    fn parses_non_negative() {
        assert_eq!(Seq::try_from(1).expect("valid").get(), 1);
        assert_eq!(Seq::try_from(i64::MAX).expect("valid").get(), i64::MAX);
    }

    #[test]
    fn rejects_negative() {
        assert!(Seq::try_from(-1).is_err());
    }

    #[test]
    fn orders_by_ordinal() {
        assert!(Seq::try_from(2).expect("v") > Seq::try_from(1).expect("v"));
    }
}
