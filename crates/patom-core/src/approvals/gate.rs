//! The hard pre-execution gate.
//!
//! The worker's tool dispatch consults this before running any tool: a gated
//! tool is denied unless a matching `approved` decision exists for the current
//! DAG. This is what makes "an agent cannot perform a gated action without an
//! approval" an enforced invariant rather than a system-prompt suggestion
//! (issue #200, locked decision 2).

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use crate::agents::AgentId;
use crate::auth::OrgId;
use crate::runtime::PromptRequestId;
use crate::types::ToolName;

use super::config::SharedGatedToolStore;
use super::store::SharedApprovalStore;

/// The gate's verdict for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Not gated, or gated and approved — run it.
    Allowed,
    /// Gated and no approval exists for the DAG — refuse, with a message the
    /// model sees so it can call `ask_approval` instead.
    Blocked(String),
}

#[async_trait]
pub trait ApprovalGate: std::fmt::Debug + Send + Sync {
    /// Decide whether `agent_id` may run `tool` in the DAG rooted at `root`.
    async fn check(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
        root: PromptRequestId,
        tool: &ToolName,
    ) -> GateOutcome;
}

/// Cheap-clone handle the agent runtime holds (optionally).
pub type SharedApprovalGate = Arc<dyn ApprovalGate>;

/// Production gate: reads the per-agent gated-tool config, then checks the
/// approval store for a matching approved decision.
#[derive(Debug, Clone)]
pub struct HardApprovalGate {
    config: SharedGatedToolStore,
    approvals: SharedApprovalStore,
}

impl HardApprovalGate {
    #[must_use]
    pub fn new(config: SharedGatedToolStore, approvals: SharedApprovalStore) -> Self {
        Self { config, approvals }
    }
}

/// The message returned to the model when a gated tool lacks approval. Names the
/// tool and points at the remedy so the next turn self-corrects.
fn blocked_message(tool: &ToolName) -> String {
    format!(
        "`{}` is an approval-gated action. Call `ask_approval` with `gated_tool` set to \
         `{}`, wait for a human to approve, then retry — it cannot run without an approved \
         decision in this conversation.",
        tool.as_str(),
        tool.as_str()
    )
}

#[async_trait]
impl ApprovalGate for HardApprovalGate {
    async fn check(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
        root: PromptRequestId,
        tool: &ToolName,
    ) -> GateOutcome {
        // Fail closed (CLAUDE.md priorities: correctness/safety first). If we
        // cannot determine whether the tool is gated, or whether an approval
        // exists, refuse — a transient DB blip must never open a gated action.
        match self.config.is_gated(org_id, agent_id, tool).await {
            Ok(false) => return GateOutcome::Allowed,
            Ok(true) => {}
            Err(e) => {
                warn!(error = %e, patom.tool = %tool.as_str(), "approval.gate.config_read_failed");
                return GateOutcome::Blocked(blocked_message(tool));
            }
        }
        match self
            .approvals
            .has_approved_for_dag(org_id, root, tool)
            .await
        {
            Ok(true) => GateOutcome::Allowed,
            Ok(false) => GateOutcome::Blocked(blocked_message(tool)),
            Err(e) => {
                warn!(error = %e, patom.tool = %tool.as_str(), "approval.gate.approval_read_failed");
                GateOutcome::Blocked(blocked_message(tool))
            }
        }
    }
}
