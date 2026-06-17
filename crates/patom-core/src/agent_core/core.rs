use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug};

use crate::auth::{Caller, OrgId};
use crate::background::{BackgroundTurnId, SharedBackgroundStore};
use crate::billing::{SharedBillingService, price_for};
use crate::clock::SharedClock;
use crate::hook::{HookChain, TurnContext};
use crate::memory::SharedMemory;
use crate::provider::{Model, OrgProviderOverlay, SharedProvider, SharedProviderRegistry};
use crate::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use crate::threads::{AgentThreadId, SharedThreadStore, ThreadId};
use crate::tools::system::todos::SharedSessionTodoStore;
use crate::tools::{SharedToolCallStore, ToolBox};
use crate::types::{AgentReply, MaxOutputTokens, MaxTurns, Participant};

use super::builder::TurnMetricsBinding;
use super::error::AgentError;
use super::limits::SUMMARIZER_INPUT_BUDGET;
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
    /// Owning org of the agent this runtime serves. Routing key into
    /// [`Self::overlay`] for the per-turn BYO-vs-platform decision (#141).
    org_id: OrgId,
    /// Per-org BYO provider overlay. Consulted live on every turn (not baked
    /// at build): a key saved after this `Agent` was cached still routes the
    /// next turn, honoring "immediate activation" (#141).
    overlay: OrgProviderOverlay,
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
    /// [`crate::billing::PgBillingService`]. When present, the turn loop checks
    /// the org's cap before each provider call and settles the turn's cost
    /// after — see [`Agent::billing_gate`] / [`Agent::billing_settle`].
    billing: Option<SharedBillingService>,
    /// Thread-feed store backing the read-at-run chat path
    /// ([`Agent::reply_in_thread`]). `None` in agent_core unit tests that
    /// do not exercise the thread path; the production factory wires
    /// [`crate::threads::PgThreadStore`].
    threads: Option<SharedThreadStore>,
    /// Background-cognition store backing [`Agent::reply_background`]. `None`
    /// outside the worker's background path.
    background: Option<SharedBackgroundStore>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        providers: SharedProviderRegistry,
        org_id: OrgId,
        overlay: OrgProviderOverlay,
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
        billing: Option<SharedBillingService>,
        threads: Option<SharedThreadStore>,
        background: Option<SharedBackgroundStore>,
    ) -> Self {
        Self {
            providers,
            org_id,
            overlay,
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
            billing,
            threads,
            background,
        }
    }

    /// Resolve this turn's provider client **and** its BYO flag in a single,
    /// **live** overlay read (#141).
    ///
    /// Returns `(client, is_byo)`: the org's BYO client + `true` when the
    /// overlay holds a usable key for [`Self::model`]'s provider, otherwise the
    /// platform registry client + `false`. Reading the overlay here (rather than
    /// baking the choice at build time) is what makes a newly-saved key route
    /// this already-cached `Agent`'s next turn — "immediate activation". The
    /// turn loop calls this **once** so routing (the client used for the send)
    /// and metering (the BYO gate/settle skip) can never disagree, even if the
    /// overlay swaps mid-turn.
    ///
    /// The client is an owned `Arc` clone (the overlay snapshot is swappable, so
    /// we cannot hand out a borrow into it). The platform-side `expect`
    /// documents the invariant: the workspace default's provider is validated at
    /// startup (`SettingsError::DefaultModelProviderNotConfigured`), and the
    /// resolver degrades any pin whose provider is neither configured nor
    /// BYO-keyed back to a routable default — so by the time a `Model` reaches
    /// here, either the org holds a BYO key for it or it is in the registry.
    pub(super) fn route(&self) -> (SharedProvider, bool) {
        if let Some(byo) = self.overlay.get(self.org_id, self.model.provider()) {
            return (byo, true);
        }
        let platform = self.providers.get(self.model.provider()).cloned().expect(
            "invariant: a Model that reaches call_provider is served either by an org \
                 BYO key or the platform registry — upheld by startup config validation + \
                 the resolver's graceful-degrade fallback",
        );
        (platform, false)
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
    pub(super) fn billing(&self) -> Option<&SharedBillingService> {
        self.billing.as_ref()
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
    /// Background-cognition store. The `expect` is a named assertion (§6):
    /// [`Agent::reply_background`] is only reachable from the worker's
    /// background claim path, which the factory always wires with a store.
    pub(super) fn background(&self) -> &SharedBackgroundStore {
        self.background.as_ref().expect(
            "invariant: reply_background requires a background store; the production agent \
             factory wires PgBackgroundStore for the cognition worker path",
        )
    }

    /// Thread store as an `Option` — used by tool-result reduction (#185), which
    /// degrades gracefully (keeps the full body in the feed) when no store is
    /// wired (agent_core unit tests), rather than asserting like [`Self::threads`].
    pub(super) fn threads_opt(&self) -> Option<&SharedThreadStore> {
        self.threads.as_ref()
    }

    /// Resolve a routable provider client for an arbitrary `model`, mirroring
    /// [`Self::route`]'s BYO-then-platform precedence. `None` if the org can call
    /// neither. Used to build the summarizer fallback chain (#185).
    pub(super) fn provider_for(&self, model: Model) -> Option<SharedProvider> {
        self.overlay
            .get(self.org_id, model.provider())
            .or_else(|| self.providers.get(model.provider()).cloned())
    }

    /// The summarizer model chain for produce-time tool-result reduction (#185):
    /// the **cheapest** model the org can actually call, followed by the cheapest
    /// model from a **different** provider as a resilience fallback (so a burned
    /// or rate-limited key fails over to another vendor). At most two entries;
    /// length one when the org has only a single usable provider.
    ///
    /// "Usable" = the model's provider is platform-configured or BYO-keyed for
    /// the org, and its context window is wide enough to hold a summarizer chunk.
    /// Cost is ranked by input price (summarization is input-heavy), tie-broken by
    /// output price.
    pub(super) fn summarizer_chain(&self) -> Vec<(Model, SharedProvider)> {
        let min_window = SUMMARIZER_INPUT_BUDGET.saturating_mul(2);
        let mut usable: Vec<Model> = Model::all()
            .filter(|m| {
                self.providers.contains(m.provider())
                    || self.overlay.has_key(self.org_id, m.provider())
            })
            .filter(|m| m.context_window().get() >= min_window)
            .collect();
        usable.sort_by_key(|m| {
            let p = price_for(*m);
            (p.input.get(), p.output.get())
        });

        let mut chain: Vec<(Model, SharedProvider)> = Vec::new();
        for model in usable {
            let Some(provider) = self.provider_for(model) else {
                continue;
            };
            match chain.first() {
                None => chain.push((model, provider)),
                // Take the cheapest model from a *different* provider as the
                // single diverse fallback, then stop.
                Some((primary, _)) if primary.provider() != model.provider() => {
                    chain.push((model, provider));
                    break;
                }
                Some(_) => {}
            }
        }
        chain
    }

    /// Drive a thread-feed turn loop for `viewer` (an agent), reading the
    /// thread context **at run time** from the [`crate::threads::ThreadStore`].
    ///
    /// There is no `prompts` argument — when the worker claims a `(thread,
    /// agent)` turn, the agent reads the thread tail itself
    /// (`context_for_agent`). The agent's reasoning / tool-call artifacts are
    /// appended to the feed as owner-private rows (shown to all, ingested only
    /// by this agent); the posted egress is the `send_message` tool.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        skip_all,
        name = "agent.reply_in_thread",
        fields(
            patom.thread.id = %thread,
            patom.state.id = %claim_key,
            patom.viewer = %viewer,
            patom.request.kind = kind_payload.kind().as_str(),
            patom.provider = self.model.provider().as_str(),
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
        // `claim_key`) is the polymorphic turn scope. The hook / tracing /
        // memory contexts key on this `ClaimKey`; the recorder rows source
        // `state_id` from the typed `claim_key` separately.
        let scope = ClaimKey::from(claim_key.as_uuid());
        let observer = observer.as_ref();
        let mut send_message_calls = 0usize;
        for turn in 0..self.max_turns.get() {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let ctx = TurnContext {
                claim_key: scope,
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

    /// Drive a **background-cognition** turn loop (reflection / resolution) for
    /// `viewer`, reading + appending to the [`crate::background::BackgroundStore`].
    ///
    /// Like [`Self::reply_in_thread`] but off the chat feed: context comes from
    /// the background turn's private log (seeded by the scheduler / librarian),
    /// the agent's exchange is appended back there, and there is **no ping-pong
    /// guard** — a cognition turn may legitimately end without `send_message`.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        skip_all,
        name = "agent.reply_background",
        fields(
            patom.background.turn.id = %turn,
            patom.viewer = %viewer,
            patom.request.kind = kind_payload.kind().as_str(),
            patom.provider = self.model.provider().as_str(),
            patom.model = %self.model,
            patom.outcome = tracing::field::Empty,
        ),
    )]
    pub async fn reply_background(
        &self,
        turn: BackgroundTurnId,
        viewer: Participant,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        let result = self
            .run_background_loop(
                turn,
                viewer,
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
    async fn run_background_loop(
        &self,
        turn: BackgroundTurnId,
        viewer: Participant,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        cancel: CancellationToken,
        observer: Option<SharedTurnObserver>,
    ) -> Result<AgentReply, AgentError> {
        // The background turn id is this turn's polymorphic scope; the
        // hook / tracing / memory contexts key on this `ClaimKey`. There is
        // no `state_id` for a cognition turn, so the recorders skip.
        let scope = ClaimKey::from(turn.as_uuid());
        let observer = observer.as_ref();
        let mut send_message_calls = 0usize;
        for turn_idx in 0..self.max_turns.get() {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let ctx = TurnContext {
                claim_key: scope,
                turn_index: turn_index(turn_idx),
            };
            let turn_span = tracing::info_span!(
                "agent.turn",
                patom.background.turn.id = %turn,
                patom.turn_index = turn_idx,
                patom.viewer = %viewer,
                patom.turn.outcome = tracing::field::Empty,
                patom.tool_calls.count = tracing::field::Empty,
            );
            let outcome = async {
                self.run_background_turn(
                    ctx,
                    turn,
                    viewer,
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
                debug!(turn_idx, "agent.background.turn.final");
                return Ok(AgentReply::new(text, send_message_calls));
            }
        }
        Err(AgentError::MaxTurnsExceeded(self.max_turns.get()))
    }
}
