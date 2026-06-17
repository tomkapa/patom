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
use crate::channels::{ChannelId, ChannelName};
use crate::colleagues::{ColleagueId, ColleagueName};
use crate::provider::ChatMessage;
use crate::runtime::PromptRequestId;
use crate::types::{MessageSender, Participant};

use super::error::ThreadError;

/// A channel a colleague belongs to — the `<channels>` context block row (#178).
#[derive(Debug, Clone)]
pub struct ChannelRef {
    pub id: ChannelId,
    pub name: ChannelName,
}

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

/// A content address for an offloaded tool-result body — the lowercase
/// SHA-256 hex of the full body (64 chars).
///
/// Newtype (CLAUDE.md §1) so a handle can't be confused with any other string,
/// and so the only ways to obtain one are hashing a body ([`content_address`])
/// or parsing a validated 64-char hex string ([`TryFrom`]).
///
/// Content-addressing makes the offload write idempotent (#185): re-running the
/// same tool call after a lease expiry recomputes the same handle, and the
/// store's `ON CONFLICT DO NOTHING` makes the second write a no-op — no
/// duplicate artifact, no double storage.
///
/// [`content_address`]: ArtifactHandle::content_address
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactHandle(String);

impl ArtifactHandle {
    /// Hash a full tool-result body into its content address. SHA-256 → 32
    /// bytes → 64 lowercase hex chars; deterministic, so identical bodies map
    /// to one handle (dedup + idempotent retry).
    #[must_use]
    pub fn content_address(body: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        Self(crate::hex::encode_32(&hasher.finalize()))
    }

    /// The raw 64-char lowercase hex address, for binding into SQL and
    /// embedding in a reduced result's recovery marker.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ArtifactHandle {
    type Error = crate::types::ParseError;
    /// Parse a handle echoed back by the model. Accepts any 64-char hex string
    /// and canonicalises to lowercase, so an upper-cased echo still resolves.
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let mut buf = [0u8; 32];
        crate::hex::decode_32(raw, &mut buf).map_err(|()| crate::types::ParseError::Malformed {
            field: "artifact_handle",
            detail: "expected 64 hex chars",
        })?;
        Ok(Self(crate::hex::encode_32(&buf)))
    }
}

/// A row to write into `tool_artifacts` — the full body of a heavy tool result,
/// offloaded out of the hot feed so the visible result can be reduced (#185).
#[derive(Debug, Clone)]
pub struct NewToolArtifact {
    pub handle: ArtifactHandle,
    pub org_id: crate::auth::OrgId,
    pub full_body: String,
    /// `chars/4` token estimate of `full_body`, for the saturation metric.
    pub tokens: i32,
    pub tool_name: crate::types::ToolName,
    /// Owner agent — the artifact is cleaned up with the agent (`ON DELETE
    /// CASCADE`). `None` on the background path with no agent participation row.
    pub agent_id: Option<AgentId>,
    pub state_id: Option<AgentThreadId>,
    pub request_id: PromptRequestId,
}

/// How a `read_artifact` call selects bytes from an offloaded body (#185).
#[derive(Debug, Clone)]
pub enum ArtifactSelector {
    /// A character window. `offset` is 0-based; `limit` is app-clamped to
    /// `MAX_ARTIFACT_SLICE` so the tool's own output can never itself exceed the
    /// reduction threshold (the recursion fixpoint).
    Page { offset: usize, limit: usize },
    /// Up to `max_matches` windows around literal occurrences of `pattern`,
    /// total output clamped to `MAX_ARTIFACT_SLICE`. Lets the agent recover "the
    /// rows after a grep match" without paging the whole body.
    Grep { pattern: String, max_matches: usize },
}

/// The bounded slice returned by [`ThreadStore::load_tool_artifact_slice`] —
/// always ≤ `MAX_ARTIFACT_SLICE` chars so re-feeding it through the dispatch
/// seam stays `Verbatim` (#185 recursion fixpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSlice(String);

impl ArtifactSlice {
    /// Wrap an already-bounded slice. The store is responsible for the clamp;
    /// this is the typed evidence that the clamp happened.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
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

/// L1 participant context for a thread: who raised it and who has spoken.
///
/// `senders` is the distinct set of colleagues that have posted, in first-seen
/// order, capped by the store. `creator` is the colleague who opened the thread
/// (may also appear in `senders`). The prompt layer dedups and enriches these
/// into the `<participants>` block.
#[derive(Debug, Clone, Default)]
pub struct ThreadParticipants {
    pub creator: Option<ColleagueId>,
    pub senders: Vec<ColleagueId>,
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

    /// Whether `colleague` (human OR agent) is a member of `channel` — the
    /// colleague-keyed authority check for an agent addressing a channel (#178).
    /// A human matches via `channel_members` (joined through its colleague); an
    /// agent matches via `channel_agent_members`. Privileged: the `(channel,
    /// colleague)` pair is fully qualified, and the channel's org bounds it.
    async fn colleague_in_channel(
        &self,
        channel: ChannelId,
        colleague: ColleagueId,
    ) -> Result<bool, ThreadError>;

    /// The channels `colleague` is a member of in `org` — the `<channels>`
    /// context block's source (#178). Union of human (`channel_members`) and
    /// agent (`channel_agent_members`) membership, active channels only,
    /// bounded by `MAX_CHANNELS_FOR_COLLEAGUE`. Privileged.
    async fn channels_for_colleague(
        &self,
        org: crate::auth::OrgId,
        colleague: ColleagueId,
    ) -> Result<Vec<ChannelRef>, ThreadError>;

    /// Record an agent `colleague` as a member of `channel` (#178). Idempotent
    /// (`ON CONFLICT DO NOTHING`). The roster sync calls this when a bot joins a
    /// mirrored chat — "bot present in chat X" ⇒ "agent member of channel X".
    /// Privileged: the write is app-keyed, not caller-authenticated.
    async fn add_agent_to_channel(
        &self,
        org: crate::auth::OrgId,
        channel: ChannelId,
        colleague: ColleagueId,
    ) -> Result<(), ThreadError>;

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

    /// L1 participant context for `thread` (issue #183): who raised it and the
    /// distinct people who have posted, for the `<participants>` prompt block.
    /// Privileged read — the agent worker is org-global within its org; the
    /// caller scopes by already holding the thread. Senders are capped.
    async fn thread_participants(
        &self,
        thread: ThreadId,
    ) -> Result<ThreadParticipants, ThreadError>;

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

    /// Offload a heavy tool-result body to the `tool_artifacts` cold store so
    /// the visible feed result can be reduced (#185). Content-addressed and
    /// write-once: an `ON CONFLICT (org_id, handle) DO NOTHING` makes a re-run
    /// after a lease expiry a no-op. Privileged write; `org_id` stamps the row
    /// for RLS.
    async fn save_tool_artifact(&self, artifact: NewToolArtifact) -> Result<(), ThreadError>;

    /// Recover an exact slice of an offloaded body on demand (#185). Returns
    /// `None` if the handle is unknown in the caller's org (RLS-scoped). The
    /// returned slice is always ≤ `MAX_ARTIFACT_SLICE` chars. Privileged read.
    async fn load_tool_artifact_slice(
        &self,
        org: crate::auth::OrgId,
        handle: &ArtifactHandle,
        selector: ArtifactSelector,
    ) -> Result<Option<ArtifactSlice>, ThreadError>;
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

#[cfg(test)]
mod artifact_handle_tests {
    use super::ArtifactHandle;

    #[test]
    fn content_address_is_stable_and_64_hex() {
        let a = ArtifactHandle::content_address("the heavy body");
        let b = ArtifactHandle::content_address("the heavy body");
        assert_eq!(
            a, b,
            "identical bodies map to one handle (idempotent retry)"
        );
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_bodies_differ() {
        let a = ArtifactHandle::content_address("body one");
        let b = ArtifactHandle::content_address("body two");
        assert_ne!(a, b);
    }

    #[test]
    fn parses_valid_hex_and_canonicalises_to_lowercase() {
        let minted = ArtifactHandle::content_address("x");
        let upper = minted.as_str().to_uppercase();
        let parsed = ArtifactHandle::try_from(upper.as_str()).expect("valid hex");
        assert_eq!(
            parsed, minted,
            "an upper-cased echo resolves to the same handle"
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(ArtifactHandle::try_from("too-short").is_err());
        assert!(ArtifactHandle::try_from("z".repeat(64).as_str()).is_err());
    }
}
