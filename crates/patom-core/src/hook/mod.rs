//! Hook system — observation and policy enforcement at every agent boundary.
//!
//! Hooks fire at four points: before/after each turn, before/after each tool call.
//! They can observe (audit, metrics) or deny (policy, rate limits, approval gates).
//! Adding cross-cutting behaviour like content filtering or per-tenant quotas means
//! writing one [`Hook`] impl, not editing `Agent::reply`.

mod dispatcher;
mod error;
mod traits;

pub use dispatcher::HookChain;
pub use error::HookError;
pub use traits::{Hook, HookDecision, HookDenied, SharedHook, ToolContext, TurnContext};
