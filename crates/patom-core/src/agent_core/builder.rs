use std::time::Duration;

use crate::agents::AgentId;
use crate::agents::prompt_versions::PromptVersionId;
use crate::background::SharedBackgroundStore;
use crate::budget::SharedBudgetService;
use crate::clock::{SharedClock, SystemClock};
use crate::hook::HookChain;
use crate::memory::SharedMemory;
use crate::provider::{Model, SharedProviderRegistry};
use crate::threads::SharedThreadStore;
use crate::tools::system::todos::SharedSessionTodoStore;
use crate::tools::{SharedToolCallStore, ToolBox, ToolRegistry};
use crate::types::{MaxOutputTokens, MaxTurns, ParseError};

use super::turn_metrics::SharedTurnMetricsStore;

use super::core::Agent;
use super::limits::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TURNS, PROVIDER_CALL_TIMEOUT, TOOL_CALL_TIMEOUT,
};

/// Composition-root builder for [`Agent`].
///
/// Required pieces (providers, memory, model) are constructor arguments;
/// everything else has a sensible default. The builder consumes itself on `build` so a
/// half-configured agent is unrepresentable.
#[derive(Debug)]
pub struct AgentBuilder {
    providers: SharedProviderRegistry,
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
    /// Thread-feed store backing the read-at-run chat path
    /// ([`Agent::reply_in_thread`]). `None` in unit tests that do not exercise
    /// the thread path; the production factory wires
    /// [`crate::threads::PgThreadStore`].
    threads: Option<SharedThreadStore>,
    /// Background-cognition store backing [`Agent::reply_background`]
    /// (reflection / resolution). `None` outside the worker's background path.
    background: Option<SharedBackgroundStore>,
}

/// Per-record identity needed to write a `turn_metrics` row. Bound at build
/// time so the recorder never has to reach back into the `AgentRecord` —
/// the factory captures both pieces once, when it materialises the Agent.
#[derive(Debug, Clone)]
pub(super) struct TurnMetricsBinding {
    pub(super) store: SharedTurnMetricsStore,
    pub(super) agent_id: AgentId,
    pub(super) prompt_version_id: PromptVersionId,
}

impl AgentBuilder {
    /// Construct a builder with mandatory pieces. Uses defaults for everything else.
    pub fn new(
        providers: SharedProviderRegistry,
        memory: SharedMemory,
        model: Model,
    ) -> Result<Self, ParseError> {
        Ok(Self {
            providers,
            memory,
            clock: SystemClock::shared(),
            tools: ToolBox::from_builtins(ToolRegistry::empty()),
            hooks: HookChain::new(),
            model,
            max_output_tokens: MaxOutputTokens::try_from(DEFAULT_MAX_OUTPUT_TOKENS)?,
            max_turns: MaxTurns::try_from(DEFAULT_MAX_TURNS)?,
            provider_timeout: PROVIDER_CALL_TIMEOUT,
            tool_timeout: TOOL_CALL_TIMEOUT,
            tool_call_store: None,
            todos_store: None,
            turn_metrics: None,
            budget: None,
            threads: None,
            background: None,
        })
    }

    /// Attach the thread-feed store that backs [`Agent::reply_in_thread`].
    ///
    /// Required for the thread-feed chat path (read-at-run context + appending
    /// the agent's private artifacts to the feed).
    #[must_use]
    pub fn with_thread_store(mut self, threads: SharedThreadStore) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Attach the background-cognition store that backs
    /// [`Agent::reply_background`] (reflection / resolution turns).
    #[must_use]
    pub fn with_background_store(mut self, background: SharedBackgroundStore) -> Self {
        self.background = Some(background);
        self
    }

    #[must_use]
    pub fn with_tools(mut self, tools: ToolBox) -> Self {
        self.tools = tools;
        self
    }

    /// Convenience: build a [`ToolBox`] from `registry` with no MCP source attached.
    /// Lets composition that doesn't care about MCP keep its existing builder chain.
    #[must_use]
    pub fn with_builtin_tools(self, registry: ToolRegistry) -> Self {
        self.with_tools(ToolBox::from_builtins(registry))
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: HookChain) -> Self {
        self.hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn with_max_output_tokens(mut self, n: MaxOutputTokens) -> Self {
        self.max_output_tokens = n;
        self
    }

    #[must_use]
    pub fn with_max_turns(mut self, n: MaxTurns) -> Self {
        self.max_turns = n;
        self
    }

    #[must_use]
    pub fn with_provider_timeout(mut self, d: Duration) -> Self {
        self.provider_timeout = d;
        self
    }

    #[must_use]
    pub fn with_tool_timeout(mut self, d: Duration) -> Self {
        self.tool_timeout = d;
        self
    }

    /// Attach the `tool_calls` audit recorder.
    ///
    /// Optional: agent_core unit tests construct an `Agent` without one and
    /// the dispatcher skips recording when absent (CLAUDE.md §6 — recording
    /// is best-effort observability, not a turn invariant). Production wires
    /// [`crate::tools::PgToolCallStore`] in via the worker pool.
    #[must_use]
    pub fn with_tool_call_store(mut self, store: SharedToolCallStore) -> Self {
        self.tool_call_store = Some(store);
        self
    }

    /// Attach the `turn_metrics` recorder + the per-record identity it
    /// stamps onto every row.
    ///
    /// Optional: agent_core unit tests build an `Agent` without one and the
    /// turn loop skips recording when absent (CLAUDE.md §6 — observability
    /// is best-effort, not on the user-visible critical path). The
    /// production factory binds [`crate::agent_core::turn_metrics::PgTurnMetricsStore`]
    /// plus the `AgentRecord`'s `(id, current_prompt_version_id)` once when
    /// the cached agent is materialised.
    #[must_use]
    pub fn with_turn_metrics(
        mut self,
        store: SharedTurnMetricsStore,
        agent_id: AgentId,
        prompt_version_id: PromptVersionId,
    ) -> Self {
        self.turn_metrics = Some(TurnMetricsBinding {
            store,
            agent_id,
            prompt_version_id,
        });
        self
    }

    /// Attach the per-org spend-budget service.
    ///
    /// Optional: agent_core unit tests skip it and the turn loop runs without a
    /// cap. The production factory binds [`crate::budget::PgBudgetService`] so
    /// every turn is gated against the org's monthly cap and its cost settled
    /// afterwards (see [`crate::agent_core::core::Agent::budget_gate`]).
    #[must_use]
    pub fn with_budget(mut self, budget: SharedBudgetService) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Attach the per-session todo store.
    ///
    /// Optional: agent_core unit tests skip it. With this wired,
    /// [`Agent::build_chat_request`] folds the current session's
    /// `<todos>` block into the system prompt so the model sees its
    /// own list at the top of every turn.
    #[must_use]
    pub fn with_todos_store(mut self, store: SharedSessionTodoStore) -> Self {
        self.todos_store = Some(store);
        self
    }

    #[must_use]
    pub fn build(self) -> Agent {
        Agent::new(
            self.providers,
            self.memory,
            self.clock,
            self.tools,
            self.hooks,
            self.model,
            self.max_output_tokens,
            self.max_turns,
            self.provider_timeout,
            self.tool_timeout,
            self.tool_call_store,
            self.todos_store,
            self.turn_metrics,
            self.budget,
            self.threads,
            self.background,
        )
    }
}
