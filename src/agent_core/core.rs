use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug};

use crate::auth::Caller;
use crate::clock::SharedClock;
use crate::hook::{HookChain, TurnContext};
use crate::memory::SharedMemory;
use crate::provider::{ChatMessage, SharedProvider, UserContent};
use crate::runtime::{PromptRequestId, RequestKindPayload};
use crate::session::{SessionError, SessionId, SharedSessionStore};
use crate::tools::system::todos::SharedSessionTodoStore;
use crate::tools::{SharedToolCallStore, ToolBox};
use crate::types::{
    AgentReply, MaxOutputTokens, MaxTurns, MessageSender, ModelId, Participant, Prompt,
};

use super::error::AgentError;
use super::observer::SharedTurnObserver;
use super::outcome::{record_reply, record_turn};
use super::turn::turn_index;

const SEND_MESSAGE_TOOL_NAME: &str = "send_message";

/// Stable name of the system tool that delivers messages — exposed to the
/// turn loop's `send_message` counter via this accessor so the constant has
/// one home.
pub(super) const fn send_message_tool_name() -> &'static str {
    SEND_MESSAGE_TOOL_NAME
}

/// The agent runtime. All collaborators live behind shared trait handles so the agent
/// is end-to-end testable with no network and any one of them can be swapped without
/// touching this struct.
#[derive(Debug, Clone)]
pub struct Agent {
    provider: SharedProvider,
    sessions: SharedSessionStore,
    memory: SharedMemory,
    clock: SharedClock,
    tools: ToolBox,
    hooks: HookChain,
    model: ModelId,
    max_output_tokens: MaxOutputTokens,
    max_turns: MaxTurns,
    provider_timeout: Duration,
    tool_timeout: Duration,
    /// Best-effort audit recorder for every dispatched tool call. `None` in
    /// agent_core unit tests; production wires
    /// [`crate::tools::PgToolCallStore`] in through the worker pool.
    tool_call_store: Option<SharedToolCallStore>,
    /// Per-session todo store. `None` in agent_core unit tests that do
    /// not exercise the per-turn context fold (the builder leaves it
    /// off when not provided); the production composition root wires
    /// [`crate::tools::system::todos::PgSessionTodoStore`] in via the
    /// agent factory. When `None`, `build_chat_request` skips the
    /// `<todos>` block entirely.
    todos_store: Option<SharedSessionTodoStore>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        provider: SharedProvider,
        sessions: SharedSessionStore,
        memory: SharedMemory,
        clock: SharedClock,
        tools: ToolBox,
        hooks: HookChain,
        model: ModelId,
        max_output_tokens: MaxOutputTokens,
        max_turns: MaxTurns,
        provider_timeout: Duration,
        tool_timeout: Duration,
        tool_call_store: Option<SharedToolCallStore>,
        todos_store: Option<SharedSessionTodoStore>,
    ) -> Self {
        Self {
            provider,
            sessions,
            memory,
            clock,
            tools,
            hooks,
            model,
            max_output_tokens,
            max_turns,
            provider_timeout,
            tool_timeout,
            tool_call_store,
            todos_store,
        }
    }

    pub(super) fn provider(&self) -> &SharedProvider {
        &self.provider
    }
    pub(super) fn sessions(&self) -> &SharedSessionStore {
        &self.sessions
    }
    pub(super) fn memory(&self) -> &SharedMemory {
        &self.memory
    }
    pub fn tools(&self) -> &ToolBox {
        &self.tools
    }
    pub(super) fn hooks(&self) -> &HookChain {
        &self.hooks
    }
    pub(super) fn model(&self) -> &ModelId {
        &self.model
    }
    pub(super) fn max_output_tokens(&self) -> MaxOutputTokens {
        self.max_output_tokens
    }
    pub(super) fn provider_timeout(&self) -> Duration {
        self.provider_timeout
    }
    pub(super) fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }
    pub(super) fn clock(&self) -> &SharedClock {
        &self.clock
    }
    pub(super) fn tool_call_store(&self) -> Option<&SharedToolCallStore> {
        self.tool_call_store.as_ref()
    }
    pub(super) fn todos_store(&self) -> Option<&SharedSessionTodoStore> {
        self.todos_store.as_ref()
    }

    /// Drive a batch of user prompts to a final assistant text answer, running
    /// tool calls in between turns. Honours `cancel` at the next checkpoint.
    /// `observer` is notified at every assistant block and tool result so the
    /// SSE pipeline streams chunks as the loop progresses.
    ///
    /// `kind` selects the per-mode `<core>` and the tool subset the model
    /// sees. `kind_payload` is the worker-supplied per-claim metadata
    /// (mirroring `prompt_requests.kind_payload`); tools that opt into
    /// kind-specific behaviour read it from [`crate::tools::ToolCallContext`].
    /// agent_core itself is variant-agnostic — it only forwards.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        skip_all,
        name = "agent.reply",
        fields(
            relay.session.id = %session,
            relay.viewer = %viewer,
            relay.request.kind = kind_payload.kind().as_str(),
            relay.provider = self.provider.name(),
            relay.model = %self.model,
            relay.batch_size = prompts.len(),
            relay.max_turns = self.max_turns.get(),
            relay.dag.root = tracing::field::Empty,
            relay.outcome = tracing::field::Empty,
        ),
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn reply(
        &self,
        session: SessionId,
        viewer: Participant,
        prompts: Vec<Prompt>,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        let result = self
            .reply_inner(
                session,
                viewer,
                prompts,
                request_id,
                caller,
                &kind_payload,
                cancel,
                observer,
            )
            .await;
        record_reply(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn reply_inner(
        &self,
        session: SessionId,
        viewer: Participant,
        prompts: Vec<Prompt>,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        assert!(!prompts.is_empty(), "reply requires at least one prompt");
        let counterpart = self.counterpart(session, viewer).await?;

        // Append once on the first call. The retry path (`resume`) re-enters
        // the loop without re-appending the same prompt rows.
        let user_blocks: Vec<UserContent> = prompts
            .into_iter()
            .map(|p| UserContent::Text(p.into_string()))
            .collect();
        self.sessions
            .append_for_user(
                caller.user_id,
                session,
                MessageSender::from_participant(counterpart),
                viewer,
                ChatMessage::User(user_blocks),
                request_id,
            )
            .await?;

        self.run_loop(
            session,
            viewer,
            counterpart,
            request_id,
            caller,
            kind_payload,
            cancel,
            observer,
        )
        .await
    }

    /// Continue an existing reply from where it left off. Used by the worker's
    /// ping-pong guard between retries — the prompt was already appended on
    /// the first `reply` call.
    #[tracing::instrument(
        skip_all,
        name = "agent.resume",
        fields(
            relay.session.id = %session,
            relay.viewer = %viewer,
            relay.request.kind = kind_payload.kind().as_str(),
            relay.provider = self.provider.name(),
            relay.model = %self.model,
            relay.max_turns = self.max_turns.get(),
            relay.dag.root = tracing::field::Empty,
            relay.outcome = tracing::field::Empty,
        ),
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn resume(
        &self,
        session: SessionId,
        viewer: Participant,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        let counterpart = self.counterpart(session, viewer).await?;
        let result = self
            .run_loop(
                session,
                viewer,
                counterpart,
                request_id,
                caller,
                &kind_payload,
                cancel,
                observer,
            )
            .await;
        record_reply(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_loop(
        &self,
        session: SessionId,
        viewer: Participant,
        counterpart: Participant,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        let viewer_as_sender = MessageSender::from_participant(viewer);
        // Resolved once per loop — constant across turns, threaded into every
        // tool call so `send_message` can bump the per-DAG budget without
        // redundant lookups.
        let root_request_id = self.sessions.root_request_id(session).await?;
        tracing::Span::current().record("relay.dag.root", tracing::field::display(root_request_id));

        let observer = observer.as_ref();
        let mut send_message_calls = 0usize;
        for turn in 0..self.max_turns.get() {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let ctx = TurnContext {
                session_id: session,
                turn_index: turn_index(turn),
            };
            let turn_span = tracing::info_span!(
                "agent.turn",
                relay.session.id = %session,
                relay.dag.root = %root_request_id,
                relay.turn_index = turn,
                relay.viewer = %viewer,
                relay.turn.outcome = tracing::field::Empty,
                relay.tool_calls.count = tracing::field::Empty,
            );
            let outcome = async {
                self.run_turn(
                    ctx,
                    viewer,
                    counterpart,
                    viewer_as_sender,
                    root_request_id,
                    request_id,
                    caller,
                    kind_payload,
                    &mut send_message_calls,
                    &cancel,
                    observer,
                )
                .await
            }
            .instrument(turn_span.clone())
            .await;
            record_turn(&turn_span, &outcome);
            if let Some(text) = outcome? {
                debug!(turn, "agent.turn.final");
                return Ok(AgentReply::new(text, send_message_calls));
            }
        }
        Err(AgentError::MaxTurnsExceeded(self.max_turns.get()))
    }

    /// Look up the counterpart participant given the explicit viewer.
    ///
    /// Sessions are 2-party. The worker passes the receiver agent as `viewer` —
    /// inferring from session ordering alone is ambiguous when both sides are
    /// agents.
    async fn counterpart(
        &self,
        session: SessionId,
        viewer: Participant,
    ) -> Result<Participant, AgentError> {
        let (a, b) = self.sessions.participants(session).await?;
        if a == viewer {
            Ok(b)
        } else if b == viewer {
            Ok(a)
        } else {
            Err(AgentError::Session(SessionError::Backend(format!(
                "agent {viewer} is not a participant of session {session}"
            ))))
        }
    }
}
