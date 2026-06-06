use std::fmt;

use crate::provider::{ChatResponse, ToolResult};

use super::error::HookError;
use super::traits::{HookDecision, SharedHook, ToolContext, TurnContext};

/// An ordered chain of hooks, evaluated short-circuit on the first `Deny`.
///
/// The chain is sealed at construction so the agent does not need to lock — every read
/// path (`before_turn`, `after_tool`, etc.) iterates an immutable slice. Add/remove
/// hooks by rebuilding the chain at composition root.
#[derive(Default, Clone)]
pub struct HookChain {
    hooks: Vec<SharedHook>,
}

impl HookChain {
    #[must_use]
    pub const fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    #[must_use]
    pub fn with(mut self, hook: SharedHook) -> Self {
        self.hooks.push(hook);
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub async fn before_turn(&self, ctx: TurnContext) -> Result<HookDecision, HookError> {
        for hook in &self.hooks {
            let decision = hook.before_turn(ctx).await?;
            if !decision.is_continue() {
                return Ok(decision);
            }
        }
        Ok(HookDecision::Continue)
    }

    pub async fn after_turn(
        &self,
        ctx: TurnContext,
        response: &ChatResponse,
    ) -> Result<HookDecision, HookError> {
        for hook in &self.hooks {
            let decision = hook.after_turn(ctx, response).await?;
            if !decision.is_continue() {
                return Ok(decision);
            }
        }
        Ok(HookDecision::Continue)
    }

    pub async fn before_tool(&self, ctx: ToolContext<'_>) -> Result<HookDecision, HookError> {
        for hook in &self.hooks {
            let decision = hook.before_tool(ctx).await?;
            if !decision.is_continue() {
                return Ok(decision);
            }
        }
        Ok(HookDecision::Continue)
    }

    pub async fn after_tool(
        &self,
        ctx: ToolContext<'_>,
        result: &ToolResult,
    ) -> Result<HookDecision, HookError> {
        for hook in &self.hooks {
            let decision = hook.after_tool(ctx, result).await?;
            if !decision.is_continue() {
                return Ok(decision);
            }
        }
        Ok(HookDecision::Continue)
    }
}

impl fmt::Debug for HookChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&'static str> = self.hooks.iter().map(|h| h.name()).collect();
        f.debug_struct("HookChain").field("hooks", &names).finish()
    }
}
