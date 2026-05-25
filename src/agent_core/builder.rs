use std::time::Duration;

use crate::clock::{SharedClock, SystemClock};
use crate::hook::HookChain;
use crate::memory::SharedMemory;
use crate::provider::{Model, SharedProviderRegistry};
use crate::session::SharedSessionStore;
use crate::tools::system::todos::SharedSessionTodoStore;
use crate::tools::{SharedToolCallStore, ToolBox, ToolRegistry};
use crate::types::{MaxOutputTokens, MaxTurns, ParseError};

use super::core::Agent;
use super::limits::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TURNS, PROVIDER_CALL_TIMEOUT, TOOL_CALL_TIMEOUT,
};

/// Composition-root builder for [`Agent`].
///
/// Required pieces (providers, sessions, memory, model) are constructor arguments;
/// everything else has a sensible default. The builder consumes itself on `build` so a
/// half-configured agent is unrepresentable.
#[derive(Debug)]
pub struct AgentBuilder {
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
}

impl AgentBuilder {
    /// Construct a builder with mandatory pieces. Uses defaults for everything else.
    pub fn new(
        providers: SharedProviderRegistry,
        sessions: SharedSessionStore,
        memory: SharedMemory,
        model: Model,
    ) -> Result<Self, ParseError> {
        Ok(Self {
            providers,
            sessions,
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
        })
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
            self.sessions,
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
        )
    }
}
