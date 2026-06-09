use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug};

use crate::auth::Caller;
use crate::budget::SharedBudgetService;
use crate::clock::SharedClock;
use crate::hook::{HookChain, TurnContext};
use crate::memory::SharedMemory;
use crate::provider::{ChatMessage, Model, SharedProviderRegistry, UserContent};
use crate::runtime::{PromptRequestId, RequestKindPayload};
use crate::session::{SessionError, SessionId, SharedSessionStore};
use crate::threads::{AgentThreadId, SharedThreadStore, ThreadId};
use crate::tools::system::todos::SharedSessionTodoStore;
use crate::tools::{SharedToolCallStore, ToolBox};
use crate::types::{AgentReply, MaxOutputTokens, MaxTurns, MessageSender, Participant, Prompt};

use super::builder::TurnMetricsBinding;
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
    providers: SharedProviderRegistry,
    sessions: SharedSessionStore,
    memory: SharedMemory,
    clock: SharedClock,
    tools: ToolBox,
    hooks: HookChain,
    model: Model,
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
    /// Best-effort recorder for `turn_metrics`, plus the identity stamped
    /// onto every row. `None` in unit tests that build an `Agent` without
    /// an `AgentRecord`; the factory at the composition root binds
    /// [`crate::agent_core::turn_metrics::PgTurnMetricsStore`] in for
    /// production. When `None` the turn loop skips the INSERT (CLAUDE.md
    /// §6: observability never blocks the user-visible turn).
    turn_metrics: Option<TurnMetricsBinding>,
    /// Per-org spend-budget seam. `None` in agent_core unit tests (the gate
    /// and settle are skipped); the production factory binds
    /// [`crate::budget::PgBudgetService`]. When present, the turn loop checks
    /// the org's cap before each provider call and settles the turn's cost
    /// after — see [`Agent::budget_gate`] / [`Agent::budget_settle`].
    budget: Option<SharedBudgetService>,
    /// Thread-feed store backing the read-at-run chat path
    /// ([`Agent::reply_in_thread`]). `None` in agent_core unit tests that
    /// only exercise the legacy pair-session path; the production factory
    /// wires [`crate::threads::PgThreadStore`].
    threads: Option<SharedThreadStore>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        providers: SharedProviderRegistry,
        sessions: SharedSessionStore,
        memory: SharedMemory,
        clock: SharedClock,
        tools: ToolBox,
        hooks: HookChain,
        model: Model,
        max_output_tokens: MaxOutputTokens,
        max_turns: MaxTurns,
        provider_timeout: Duration,
        tool_timeout: Duration,
        tool_call_store: Option<SharedToolCallStore>,
        todos_store: Option<SharedSessionTodoStore>,
        turn_metrics: Option<TurnMetricsBinding>,
        budget: Option<SharedBudgetService>,
        threads: Option<SharedThreadStore>,
    ) -> Self {
        Self {
            providers,
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
            turn_metrics,
            budget,
            threads,
        }
    }

    /// Routing-time provider lookup. Returns the [`crate::provider::SharedProvider`]
    /// that serves [`Self::model`]'s `provider()` discriminant. The
    /// `expect` documents the invariant: the workspace default's provider is
    /// validated at startup
    /// (`SettingsError::DefaultModelProviderNotConfigured`), and
    /// [`crate::agents::StaticAgentModelResolver`] degrades any per-agent
    /// pin whose provider has since been dropped from config back to that
    /// default — so by the time a `Model` reaches this getter, its provider
    /// is known to be in the registry. A `None` here means that invariant
    /// was bypassed (custom resolver, in-memory mutation) and is an
    /// operational fault we surface immediately.
    pub(super) fn provider(&self) -> &crate::provider::SharedProvider {
        self.providers.get(self.model.provider()).expect(
            "invariant: registry contains the provider for every Model that reaches \
             call_provider — upheld by startup config validation + the resolver's \
             graceful-degrade fallback to the workspace default",
        )
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
    pub(super) fn model(&self) -> Model {
        self.model
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
    pub(super) fn turn_metrics(&self) -> Option<&TurnMetricsBinding> {
        self.turn_metrics.as_ref()
    }
    pub(super) fn budget(&self) -> Option<&SharedBudgetService> {
        self.budget.as_ref()
    }
    /// Thread-feed store for the read-at-run chat path. The `expect` is a named
    /// assertion (CLAUDE.md §6): [`Agent::reply_in_thread`] is only reachable
    /// from the worker's thread-feed claim path, which the production factory
    /// always wires with a store. A `None` here means an agent built for the
    /// legacy pair path was driven down the thread path — a wiring bug.
    pub(super) fn threads(&self) -> &SharedThreadStore {
        self.threads.as_ref().expect(
            "invariant: reply_in_thread requires a thread store; the production agent \
             factory wires PgThreadStore for the thread-feed worker path",
        )
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
            patom.session.id = %session,
            patom.viewer = %viewer,
            patom.request.kind = kind_payload.kind().as_str(),
            patom.provider = self.provider().name(),
            patom.model = %self.model,
            patom.batch_size = prompts.len(),
            patom.max_turns = self.max_turns.get(),
            patom.dag.root = tracing::field::Empty,
            patom.outcome = tracing::field::Empty,
        ),
    )]
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
                MessageSender::from(counterpart),
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
            patom.session.id = %session,
            patom.viewer = %viewer,
            patom.request.kind = kind_payload.kind().as_str(),
            patom.provider = self.provider().name(),
            patom.model = %self.model,
            patom.max_turns = self.max_turns.get(),
            patom.dag.root = tracing::field::Empty,
            patom.outcome = tracing::field::Empty,
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

    /// Drive a thread-feed turn loop for `viewer` (an agent), reading the
    /// thread context **at run time** from the [`crate::threads::ThreadStore`].
    ///
    /// The thread-feed analogue of [`Self::reply`]: there is no `prompts`
    /// argument — when the worker claims a `(thread, agent)` turn, the agent
    /// reads the thread tail itself (`context_for_agent`). The agent's
    /// reasoning / tool-call artifacts are appended to the feed as
    /// owner-private rows (shown to all, ingested only by this agent); the
    /// posted egress is the `send_message` tool, wired in a later phase.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        skip_all,
        name = "agent.reply_in_thread",
        fields(
            patom.thread.id = %thread,
            patom.state.id = %claim_key,
            patom.viewer = %viewer,
            patom.request.kind = kind_payload.kind().as_str(),
            patom.provider = self.provider().name(),
            patom.model = %self.model,
            patom.max_turns = self.max_turns.get(),
            patom.outcome = tracing::field::Empty,
        ),
    )]
    pub async fn reply_in_thread(
        &self,
        claim_key: AgentThreadId,
        thread: ThreadId,
        viewer: Participant,
        request_id: PromptRequestId,
        root_request_id: PromptRequestId,
        caller: Caller,
        kind_payload: RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        let result = self
            .run_thread_loop(
                claim_key,
                thread,
                viewer,
                request_id,
                root_request_id,
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
    async fn run_thread_loop(
        &self,
        claim_key: AgentThreadId,
        thread: ThreadId,
        viewer: Participant,
        request_id: PromptRequestId,
        root_request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        // In the thread model the agent's participation id (`state_id` =
        // `claim_key`) is the turn-scope identity. The hook / tracing contexts
        // that still speak `SessionId` carry it bridged through `SessionId::from`
        // — `turn_metrics.state_id` / `tool_calls.state_id` are this same id.
        let scope = SessionId::from(claim_key.as_uuid());
        let observer = observer.as_ref();
        let mut send_message_calls = 0usize;
        for turn in 0..self.max_turns.get() {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let ctx = TurnContext {
                session_id: scope,
                turn_index: turn_index(turn),
            };
            let turn_span = tracing::info_span!(
                "agent.turn",
                patom.thread.id = %thread,
                patom.state.id = %claim_key,
                patom.turn_index = turn,
                patom.viewer = %viewer,
                patom.turn.outcome = tracing::field::Empty,
                patom.tool_calls.count = tracing::field::Empty,
            );
            let outcome = async {
                self.run_thread_turn(
                    ctx,
                    claim_key,
                    thread,
                    viewer,
                    request_id,
                    root_request_id,
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
                debug!(turn, "agent.thread.turn.final");
                return Ok(AgentReply::new(text, send_message_calls));
            }
        }
        Err(AgentError::MaxTurnsExceeded(self.max_turns.get()))
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
        let viewer_as_sender = MessageSender::from(viewer);
        // Resolved once per loop — constant across turns, threaded into every
        // tool call so `send_message` can bump the per-DAG budget without
        // redundant lookups.
        let root_request_id = self.sessions.root_request_id(session).await?;
        tracing::Span::current().record("patom.dag.root", tracing::field::display(root_request_id));

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
                patom.session.id = %session,
                patom.dag.root = %root_request_id,
                patom.turn_index = turn,
                patom.viewer = %viewer,
                patom.turn.outcome = tracing::field::Empty,
                patom.tool_calls.count = tracing::field::Empty,
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
