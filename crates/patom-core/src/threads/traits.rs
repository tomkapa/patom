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

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::auth::{Caller, UserId};
use crate::channels::ChannelId;
use crate::colleagues::ColleagueId;
use crate::provider::ChatMessage;
use crate::runtime::PromptRequestId;

use super::error::ThreadError;

/// A thread's listing row — the membership-scoped feed index (P7).
///
/// Carries just the fields a channel/DM list needs; the per-thread message feed
/// is read separately via [`ThreadStore::context_for_agent`] / the HTTP feed read.
#[derive(Debug, Clone)]
pub struct ThreadListItem {
    pub thread_id: ThreadId,
    pub channel_id: Option<ChannelId>,
    pub last_activity_at: DateTime<Utc>,
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
}

/// Storage trait for the thread feed. Implementations must be thread-safe.
#[async_trait]
pub trait ThreadStore: fmt::Debug + Send + Sync {
    /// Create a thread. `channel_id = None` => a DM thread. `root_message_id`
    /// is the channel-timeline message this reply-thread hangs under (None for
    /// the channel's own timeline thread + DMs). Tenant-scoped: the row's
    /// `org_id` comes from `caller` and is gated by the RLS WITH CHECK.
    async fn create_thread(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
        root_message_id: Option<ThreadMessageId>,
        created_by: ColleagueId,
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
    /// created them (P7). `channel_id = Some` ⇒ that channel's threads, gated on
    /// the caller being a `channel_members` row and the channel not archived.
    /// `channel_id = None` ⇒ the caller's DMs (`threads.channel_id IS NULL`,
    /// created by the caller). Org-pinned (`caller.org_id`) so a multi-org
    /// member's other workspaces never leak in (RLS gates membership, not the
    /// active org). Ordered newest-activity first.
    async fn list_threads(
        &self,
        caller: &Caller,
        channel_id: Option<ChannelId>,
    ) -> Result<Vec<ThreadListItem>, ThreadError>;

    /// Whether `user_id` may receive a posted message in `thread` — the
    /// `send_message` human gate (no auto-add).
    ///
    /// A channel thread requires the human to be in `channel_members`; a DM
    /// thread (`channel_id IS NULL`) is private to its creator, so its human is
    /// always reachable. Returns [`ThreadError::NotFound`] if the thread is
    /// missing. Privileged read — agents are org-global and gate humans by
    /// membership, not by the acting principal.
    async fn is_channel_member(
        &self,
        thread: ThreadId,
        user_id: UserId,
    ) -> Result<bool, ThreadError>;

    /// Build `agent`'s LLM context for `thread`: every `posted` row (from
    /// anyone) plus `agent`'s own private artifacts, in `seq` order, mapped to
    /// `viewer`'s perspective (own utterances → Assistant, others → User).
    /// Peers' private rows are excluded. Privileged read — an agent is
    /// org-global within its org.
    async fn context_for_agent(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
    ) -> Result<Vec<ChatMessage>, ThreadError>;
}

/// Cheap-clone handle so consumers hold the store without a generic parameter.
pub type SharedThreadStore = Arc<dyn ThreadStore>;
