//! Provider-agnostic agent runtime.
//!
//! The `Agent` is the orchestrator: provider + sessions + memory + hooks + tools wired
//! into a tool-using chat loop. It owns no I/O of its own — every external call goes
//! through one of the trait objects so the agent is testable end-to-end without a
//! network.

mod builder;
mod core;
mod error;
mod limits;
mod log;
mod observer;
mod outcome;
mod turn;
pub mod turn_metrics;

pub use builder::AgentBuilder;
pub use core::Agent;
pub use error::AgentError;
pub use limits::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TURNS, MAX_TOOL_CALLS_PER_TURN, MAX_TURN_LIST_PAGE_SIZE,
    MAX_TURNS_PER_TIMESERIES_RESPONSE, PROVIDER_CALL_TIMEOUT, TOOL_CALL_TIMEOUT,
};
pub use observer::{NoopObserver, SharedTurnObserver, TurnObserver};
