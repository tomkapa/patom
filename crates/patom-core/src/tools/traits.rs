use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::auth::{OrgId, UserId};
use crate::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use crate::threads::{AgentThreadId, ThreadId};
use crate::types::{Participant, ToolName};

use super::modes::RequestKindModes;
use super::url::UrlError;

#[derive(Debug, Error)]
pub enum ToolError {
    /// Model gave us bad arguments — wrong shape, oversize, refers to a
    /// non-existent receiver, etc. Surfacing as `invalid_input` lets the
    /// model self-correct on the next turn.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A downstream subsystem (session store, queue, sink, agent store) the
    /// tool depends on failed in a way that is *not* the model's fault. Kept
    /// distinct from `InvalidInput` so dashboards can separate model-driven
    /// errors from infrastructure-driven ones, and so a future retry policy
    /// can target backend faults without retrying bad-input rejections.
    #[error("backend error: {0}")]
    Backend(String),

    #[error("disallowed url: {0}")]
    DisallowedUrl(#[from] UrlError),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("upstream returned status {status}: {body}")]
    Upstream { status: u16, body: String },

    #[error("tool returned a result that exceeded the size cap ({max} bytes)")]
    ResultTooLarge { max: usize },

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Per-call context passed to tools that need to know who's calling them.
///
/// Threaded by the agent loop into [`Tool::execute`]. Most tools
/// (`web_fetch`, `web_search`, MCP tools) ignore it; system tools
/// (`send_message`, `get_session`, memory tools) consume it.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    /// Polymorphic turn scope for this tool call — `agent_thread_state.id`
    /// (chat) or `background_turn_id` (background). The memory / hook / tracing
    /// contexts key on this; recorder rows source their FK from [`state_id`]
    /// instead, which is `None` on the background path.
    ///
    /// [`state_id`]: Self::state_id
    pub claim_key: ClaimKey,
    /// Thread the tool call belongs to, in the thread-feed chat path. `None` on
    /// the background-cognition path. `send_message` posts the egress row here.
    pub thread_id: Option<ThreadId>,
    /// The agent's participation id (`agent_thread_state.id`) for this turn —
    /// the recorder FK (`turn_metrics.state_id` / `tool_calls.state_id` /
    /// `session_todos.state_id`). `None` on the background path (no
    /// `agent_thread_state` row), where recording is skipped.
    pub state_id: Option<AgentThreadId>,
    /// The agent currently running — its identity is what `send_message`'s
    /// receiver is checked against and what authors any messages the tool
    /// appends.
    pub viewer: Participant,
    /// DAG anchor for the conversation tree this turn belongs to. Used by
    /// `send_message` to upsert sibling sessions and bump the budget.
    pub root_request_id: PromptRequestId,
    /// The current claim's prompt request id — i.e. the row whose SSE sink
    /// is open right now. Used by `send_message` to publish
    /// `AgentMessage` chunks where the human is actually listening, instead
    /// of `root_request_id` (which can point at a long-since-quiesced
    /// prompt's closed sink on follow-up turns in a continuing thread).
    /// Postgres `LISTEN/NOTIFY` then routes the chunk by
    /// `prompt_requests.root_request_id` to the right thread fan-in.
    pub request_id: PromptRequestId,
    /// Kind-specific metadata for the active claim, copied from
    /// `prompt_requests.kind_payload`. Always present — `Normal` claims
    /// carry the empty [`RequestKindPayload::Normal`] variant. Tools that
    /// opt into kind-specific behaviour (the memory mutation tools close
    /// the active contradiction during a resolution claim) pattern-match
    /// this. Carrying the whole enum — instead of a per-variant scalar —
    /// keeps agent_core ignorant of which variants exist; new payload
    /// variants are added without touching the turn loop.
    pub kind_payload: RequestKindPayload,
    /// Human at the DAG root of the claimed session. Threaded from the
    /// worker pool (`ClaimedSession.created_by_user_id`) into every
    /// tool call so the store mutations can open a `begin_as_user`
    /// transaction and the database RLS WITH CHECK fires against the
    /// right principal — a worker that tried to write into a foreign
    /// org's tables would be rejected at the boundary.
    pub acting_user_id: UserId,
    /// Organization that owns the claimed session. Threaded from the
    /// worker pool (`ClaimedSession.org_id`); used by the dispatcher's
    /// `tool_calls` recorder to denormalise org on the audit row
    /// (matches the parent session's org — the BEFORE INSERT trigger
    /// in migration 25 enforces equality as defence in depth).
    pub org_id: OrgId,
}

/// The intent a [`ToolResultPolicy::Summarize`] pass is keyed to (#185).
///
/// A bounded hint — the WebFetch `prompt`, or a serialization of the tool call
/// input — that tells the cheap-model extractive fold *what to keep*. Clamped
/// rather than rejected: a too-long hint is truncated (it only steers salience;
/// losing the tail of a hint is harmless), mirroring `CompactionSummary::clamp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReductionIntent(String);

impl ReductionIntent {
    /// Build an intent from raw text, truncated to `MAX_REDUCTION_INTENT_CHARS`
    /// on a char boundary.
    #[must_use]
    pub fn clamp(mut text: String) -> Self {
        super::limits::truncate_to_char_boundary(
            &mut text,
            super::limits::MAX_REDUCTION_INTENT_CHARS,
        );
        Self(text)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the dispatch seam reduces an *oversized* tool result (#185).
///
/// Consulted only once a result exceeds `TOOL_RESULT_REDUCE_THRESHOLD`; smaller
/// results never reach this. Both variants offload the full body to
/// `tool_artifacts` first (lossless) — they differ only in the visible body the
/// feed keeps.
#[derive(Debug, Clone)]
pub enum ToolResultPolicy {
    /// Lossless, zero-LLM: visible body = head + tail + handle; the agent pages
    /// the rest via `read_artifact`. Default for opaque results (e.g. MCP).
    Paginate,
    /// Cheap-model extractive fold keyed to `intent`, stored as the visible
    /// body, with the handle for exact recovery. For web fetch / tagged tools.
    Summarize { intent: ReductionIntent },
}

/// A side-effecting capability the model can request.
///
/// Implementations must be cheap to clone (they go behind `Arc`) and must validate every
/// input from the model — never trust `input: Value` shape, parse it through `serde` into
/// a typed struct.
#[async_trait]
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// Stable, lower-case identifier. Validated at registration through [`ToolName`].
    fn name(&self) -> &ToolName;

    /// Human-readable description shown to the model. Be specific — vague descriptions
    /// produce vague tool calls.
    fn description(&self) -> &str;

    /// JSON-schema description of the tool's input. Cached on the tool struct so the
    /// agent does not re-allocate it every turn.
    fn input_schema(&self) -> Arc<Value>;

    /// Invoke the tool. Stateless tools (`web_fetch`, `web_search`, MCP
    /// wrappers) ignore `ctx`; system tools (`send_message`, `get_session`,
    /// memory tools) consume it for authorship, scoping, and per-turn caps.
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError>;

    /// Modes (request kinds) this tool participates in. Defaults to
    /// every mode — opt out only when a tool is genuinely meaningless
    /// or unsafe in a given mode. The agent's per-turn chat-request
    /// builder filters specs by `kind`, and the dispatcher refuses to
    /// invoke a tool whose `modes()` excludes the active kind.
    fn modes(&self) -> RequestKindModes {
        RequestKindModes::ALL
    }

    /// `true` if the tool has no observable side effects on patom state
    /// (sessions, memory, agents, schedules, MCP-server state) and is
    /// safe to invoke concurrently with any other `concurrency_safe`
    /// tool. Default `false`: a new tool serialises unless its author
    /// has reasoned about safety.
    fn concurrency_safe(&self) -> bool {
        false
    }

    /// How an *oversized* result from this tool is reduced at the dispatch seam
    /// (#185). Consulted only when the result exceeds
    /// `TOOL_RESULT_REDUCE_THRESHOLD`; `input` is the same value passed to
    /// [`execute`](Tool::execute), so a tool can vary the policy by call (e.g.
    /// WebFetch summarizes only when the model supplied a `prompt`).
    ///
    /// Default [`Paginate`](ToolResultPolicy::Paginate): a lossless, zero-LLM
    /// head+tail+handle preview — right for opaque payloads whose structure we
    /// can't assume (the dominant MCP case).
    fn result_policy(&self, _input: &Value) -> ToolResultPolicy {
        ToolResultPolicy::Paginate
    }
}

/// Cheap-clone alias used by the registry.
pub type SharedTool = Arc<dyn Tool>;
