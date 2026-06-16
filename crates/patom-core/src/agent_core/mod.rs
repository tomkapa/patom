//! Provider-agnostic agent runtime.
//!
//! The `Agent` is the orchestrator: provider + sessions + memory + hooks + tools wired
//! into a tool-using chat loop. It owns no I/O of its own — every external call goes
//! through one of the trait objects so the agent is testable end-to-end without a
//! network.

mod builder;
mod compaction;
mod core;
mod error;
mod limits;
mod log;
mod observer;
mod outcome;
mod turn;
pub mod turn_metrics;

pub use builder::AgentBuilder;
pub use compaction::{
    AgentContext, CompactionError, CompactionSummary, TokenEstimate, cut_at_tool_safe_boundary,
    estimate_tokens, tool_safe_cut_index,
};
pub use core::Agent;
pub use error::AgentError;
pub use limits::{
    COMPACTION_COOLDOWN, COMPACTION_LLM_TIMEOUT, CONTEXT_TOKEN_BUDGET_DIVISOR,
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TURNS, MAX_COMPACTION_CHUNKS, MAX_COMPACTION_WALL_CLOCK,
    MAX_HOOKS_PER_TURN, MAX_SUMMARY_TOKENS, MAX_TOOL_CALLS_PER_TURN, MAX_TURN_LIST_PAGE_SIZE,
    MAX_TURNS_PER_TIMESERIES_RESPONSE, PROVIDER_CALL_TIMEOUT, SUMMARIZER_INPUT_BUDGET,
    TOOL_CALL_TIMEOUT,
};
pub use observer::{NoopObserver, SharedTurnObserver, TurnObserver};
