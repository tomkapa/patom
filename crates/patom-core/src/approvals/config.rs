//! Per-agent gated-tool configuration.
//!
//! Which tool names require approval is *data*, not code: a static
//! `requires_approval()` marker could not cover tenant-specific MCP tools
//! (`refund_customer`, `merge_pr`). Both the hard gate and the system-prompt
//! builder read this seam.

use async_trait::async_trait;

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId};
use crate::types::ToolName;

use super::error::ApprovalError;

#[async_trait]
pub trait GatedToolStore: std::fmt::Debug + Send + Sync {
    /// Does this agent gate this tool? The hard gate's fast, indexed primary-key
    /// lookup. Privileged (runs on the worker dispatch path).
    async fn is_gated(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<bool, ApprovalError>;

    /// Every gated tool name for an agent — rendered into the agent's
    /// `<approval-gated-tools>` system-prompt block. Privileged.
    async fn gated_tools_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Vec<ToolName>, ApprovalError>;

    /// Admin: mark a tool as approval-gated for an agent (idempotent).
    /// Tenant-side (the admin has a `Caller`).
    async fn set_gated(
        &self,
        caller: &Caller,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<(), ApprovalError>;

    /// Admin: clear the gate for a tool (idempotent).
    async fn unset_gated(
        &self,
        caller: &Caller,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<(), ApprovalError>;
}

/// Cheap-clone alias held by the gate and the memory/prompt builder.
pub type SharedGatedToolStore = std::sync::Arc<dyn GatedToolStore>;
