use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::provider::{ChatResponse, ToolCall, ToolResult};
use crate::session::SessionId;
use crate::types::TurnIndex;

use super::error::HookError;

/// Error type produced by [`HookDecision::into_result`] when a hook denies
/// the operation. Convertible to [`crate::agent_core::AgentError`] so callers can
/// propagate with `?`.
#[derive(Debug, Error)]
#[error("hook denied: {0}")]
pub struct HookDenied(pub String);

/// What a hook decides about an operation it just observed.
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// Allow the agent to proceed.
    Continue,
    /// Reject the operation. The agent will surface this as an error and stop.
    Deny { reason: String },
}

impl HookDecision {
    #[must_use]
    pub const fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }

    /// Convert into a `Result` so callers can `?`-propagate denials. `Continue`
    /// becomes `Ok(())`; `Deny` becomes `Err(HookDenied(reason))`.
    pub fn into_result(self) -> Result<(), HookDenied> {
        match self {
            Self::Continue => Ok(()),
            Self::Deny { reason } => Err(HookDenied(reason)),
        }
    }
}

/// Read-only snapshot of agent state at a turn boundary.
#[derive(Debug, Clone, Copy)]
pub struct TurnContext {
    pub session_id: SessionId,
    pub turn_index: TurnIndex,
}

/// Read-only snapshot of agent state at a tool boundary.
#[derive(Debug, Clone, Copy)]
pub struct ToolContext<'a> {
    pub session_id: SessionId,
    pub turn_index: TurnIndex,
    pub call: &'a ToolCall,
}

/// Plug-in point for cross-cutting behaviour. All methods default to a no-op
/// `Continue` so impls only override what they care about.
#[async_trait]
pub trait Hook: Send + Sync + fmt::Debug {
    /// Stable, low-cardinality name for tracing fields.
    fn name(&self) -> &'static str;

    async fn before_turn(&self, _ctx: TurnContext) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Continue)
    }

    async fn after_turn(
        &self,
        _ctx: TurnContext,
        _response: &ChatResponse,
    ) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Continue)
    }

    async fn before_tool(&self, _ctx: ToolContext<'_>) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Continue)
    }

    async fn after_tool(
        &self,
        _ctx: ToolContext<'_>,
        _result: &ToolResult,
    ) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Continue)
    }
}

/// Reference-counted handle for cheap cloning into the agent / dispatcher.
pub type SharedHook = Arc<dyn Hook>;
