//! `ask_approval` — the agent's checkpoint before a consequential, gated action
//! (issue #200).
//!
//! The tool records a `pending_approval` row, posts a user-visible approval
//! request to the thread, and RETURNS — the worker is run-to-completion with no
//! wait state, so the turn ends cleanly. A later human decision (Discord button /
//! Lark card / web) authorizes + atomically resolves the row and enqueues a
//! fresh trigger that resumes this agent with the decision seeded; the agent
//! re-attempts the gated tool, which the hard pre-execution gate now allows.
//!
//! `ask_approval` is *egress* (it posts a card the human sees), so the turn loop
//! counts it toward the ping-pong guard via `tool_is_egress` — an
//! `ask_approval`-only turn is not falsely failed `NoEgress`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::approvals::{
    ActionSummary, ApprovalId, ApproverPolicy, NewApproval, PlatformTarget, SharedApprovalStore,
};
use crate::auth::Caller;
use crate::clock::SharedClock;
use crate::colleagues::{ColleagueError, ColleagueId, SharedColleagueStore};
use crate::outbound::SharedOutboundRouter;
use crate::provider::{ChatMessage, UserContent};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore, ThreadId};
use crate::types::ToolName;

use super::super::traits::{Tool, ToolCallContext, ToolError};
use crate::approvals::limits::{APPROVAL_DEFAULT_TTL, APPROVAL_MAX_TTL, MAX_APPROVERS};

const TOOL_NAME: &str = "ask_approval";

const TOOL_DESCRIPTION: &str = "Request human approval before performing a consequential, \
    approval-gated action. Use this when a tool you intend to call is approval-gated (you will \
    be told which). Arguments: `action` is a clear one- or two-sentence description of what you \
    want to do, shown to the approver (e.g. \"Refund $40 to customer #12 for the duplicate \
    charge\"). `gated_tool` is the exact name of the tool this authorizes. `approvers` \
    (optional) is a list of colleague ids (uuids) from your <colleagues> block who may decide; \
    omit it to let anyone in the org approve. `expires_in_secs` (optional) bounds how long the \
    request stays open. This POSTS an approval request and then your turn ends — you will be \
    woken again with the decision. Do NOT call the gated tool until you are told it was approved.";

#[derive(Debug, Deserialize)]
struct AskApprovalInput {
    /// Human-readable description of the gated action.
    action: String,
    /// The exact tool name this approval authorizes.
    gated_tool: String,
    /// Colleague ids permitted to decide. Omitted/empty ⇒ anyone in the org.
    #[serde(default)]
    approvers: Vec<ColleagueId>,
    /// Optional TTL override (clamped to `APPROVAL_MAX_TTL`).
    #[serde(default)]
    expires_in_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AskApprovalOutput {
    approval_id: ApprovalId,
    status: &'static str,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Approval checkpoint tool.
///
/// Holds the approval store (record + dedupe), the thread store (post the
/// request), colleagues (validate approvers), the outbound router (reach the
/// external surface), and the clock (TTL).
pub struct AskApprovalTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    approvals: SharedApprovalStore,
    threads: SharedThreadStore,
    colleagues: SharedColleagueStore,
    outbound: SharedOutboundRouter,
    clock: SharedClock,
}

impl std::fmt::Debug for AskApprovalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskApprovalTool").finish_non_exhaustive()
    }
}

impl AskApprovalTool {
    #[must_use]
    pub fn new(
        approvals: SharedApprovalStore,
        threads: SharedThreadStore,
        colleagues: SharedColleagueStore,
        outbound: SharedOutboundRouter,
        clock: SharedClock,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: ask_approval is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["action", "gated_tool"],
            "properties": {
                "action": { "type": "string", "minLength": 1 },
                "gated_tool": { "type": "string", "minLength": 1, "maxLength": 64 },
                "approvers": {
                    "type": "array",
                    "items": { "type": "string", "format": "uuid" },
                    "maxItems": MAX_APPROVERS
                },
                "expires_in_secs": { "type": "integer", "minimum": 1 }
            },
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            approvals,
            threads,
            colleagues,
            outbound,
            clock,
        }
    }

    /// Validate every approver colleague is a human in the caller's org. A
    /// cross-org or unknown id is reported as invalid input (no existence leak);
    /// an agent id is rejected — only humans approve.
    async fn validate_approvers(
        &self,
        approvers: Vec<ColleagueId>,
        ctx: &ToolCallContext,
    ) -> Result<ApproverPolicy, ToolError> {
        for id in &approvers {
            let colleague = self.colleagues.read(*id).await.map_err(|e| match e {
                ColleagueError::NotFound(_) => {
                    ToolError::InvalidInput(format!("ask_approval: unknown approver {id}"))
                }
                err => ToolError::Backend(format!("ask_approval: approver lookup: {err}")),
            })?;
            if colleague.org_id() != ctx.org_id {
                return Err(ToolError::InvalidInput(format!(
                    "ask_approval: unknown approver {id}"
                )));
            }
            if colleague.user_id().is_none() {
                return Err(ToolError::InvalidInput(format!(
                    "ask_approval: approver {id} is an agent; only humans can approve"
                )));
            }
        }
        ApproverPolicy::from_ids(approvers)
            .map_err(|e| ToolError::InvalidInput(format!("ask_approval: {e}")))
    }

    /// Clamp the requested TTL into `[_, APPROVAL_MAX_TTL]` and project it to an
    /// absolute deadline off the injected clock (CLAUDE.md §11).
    fn deadline(&self, expires_in_secs: Option<u64>) -> chrono::DateTime<chrono::Utc> {
        let requested =
            expires_in_secs.map_or(APPROVAL_DEFAULT_TTL, std::time::Duration::from_secs);
        let ttl = requested.min(APPROVAL_MAX_TTL);
        // Named assertion (§6): `ttl` is clamped to APPROVAL_MAX_TTL (one week),
        // far within `chrono::Duration`'s range (`from_std` only fails past
        // ~292e9 years), so the conversion is infallible here.
        let delta = chrono::Duration::from_std(ttl)
            .expect("invariant: clamped ttl <= APPROVAL_MAX_TTL fits chrono::Duration");
        self.clock.now_utc() + delta
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.ask_approval",
        fields(
            patom.thread.id = tracing::field::Empty,
            patom.from.viewer = %ctx.viewer,
            patom.approval.id = tracing::field::Empty,
        ),
    )]
    async fn handle(
        &self,
        input: AskApprovalInput,
        ctx: &ToolCallContext,
    ) -> Result<AskApprovalOutput, ToolError> {
        let agent_id = ctx.viewer.agent_id().ok_or_else(|| {
            ToolError::InvalidInput("ask_approval: caller must be an agent".into())
        })?;
        let requesting_colleague = ctx.viewer.colleague_id().ok_or_else(|| {
            ToolError::InvalidInput("ask_approval: caller must be a colleague".into())
        })?;
        let thread = ctx.thread_id.ok_or_else(|| {
            ToolError::Backend("ask_approval: no thread context on this call".into())
        })?;
        tracing::Span::current().record("patom.thread.id", tracing::field::display(thread));

        let action = ActionSummary::try_from(input.action)
            .map_err(|e| ToolError::InvalidInput(format!("ask_approval: action: {e}")))?;
        let gated_tool = ToolName::try_from(input.gated_tool.as_str())
            .map_err(|e| ToolError::InvalidInput(format!("ask_approval: gated_tool: {e}")))?;
        let approvers = self.validate_approvers(input.approvers, ctx).await?;
        let expires_at = self.deadline(input.expires_in_secs);

        let key = idempotency_key(thread, ctx.root_request_id, &gated_tool, action.as_str());
        let caller = Caller::new(ctx.acting_user_id, ctx.org_id);
        let outcome = self
            .approvals
            .create(
                &caller,
                NewApproval {
                    id: ApprovalId::new(),
                    thread_id: thread,
                    requesting_agent_id: agent_id,
                    requesting_colleague_id: requesting_colleague,
                    root_request_id: ctx.root_request_id,
                    action_summary: action.clone(),
                    gated_tool,
                    approvers,
                    // v1 posts to the current thread (web/in-thread). The Discord
                    // / Lark posters supply their interactive binding when wired.
                    target: PlatformTarget::Web,
                    idempotency_key: key,
                    expires_at,
                },
            )
            .await
            .map_err(|e| ToolError::Backend(format!("ask_approval: create: {e}")))?;
        let record = outcome.record();
        tracing::Span::current().record("patom.approval.id", tracing::field::display(record.id));

        // Post the visible request to the thread, then ensure it reaches the
        // external surface (Lark/Discord) — best-effort, like send_message.
        self.post_request(&caller, thread, requesting_colleague, &action, ctx)
            .await?;
        let _ = self.outbound.ensure_delivery(ctx.org_id, thread).await;

        info!(
            patom.approval.id = %record.id,
            patom.agent.id = %agent_id,
            "ask_approval.requested",
        );
        Ok(AskApprovalOutput {
            approval_id: record.id,
            status: "pending",
            expires_at,
        })
    }

    /// Append the user-visible approval request to the thread feed, authored by
    /// the requesting agent.
    async fn post_request(
        &self,
        caller: &Caller,
        thread: ThreadId,
        sender: ColleagueId,
        action: &ActionSummary,
        ctx: &ToolCallContext,
    ) -> Result<(), ToolError> {
        let body = format!(
            "🔔 Approval needed: {}. Awaiting a human decision before I proceed.",
            action.as_str()
        );
        self.threads
            .append(
                caller,
                thread,
                NewMessage {
                    kind: MessageKind::Posted,
                    sender: Some(sender),
                    owner_agent_id: None,
                    receiver: None,
                    body: ChatMessage::User(vec![UserContent::Text(body)]),
                    request_id: Some(ctx.request_id),
                    idempotency_key: None,
                },
            )
            .await
            .map_err(|e| {
                warn!(error = %e, "ask_approval.post_failed");
                ToolError::Backend(format!("ask_approval: post failed: {e}"))
            })?;
        Ok(())
    }
}

/// Deterministic dedupe key for repeated `ask_approval` calls in one DAG:
/// `apv:{thread}:{root}:{gated_tool}:{sha256(action)[..16]}`. A stable hash (not
/// `DefaultHasher`, which is process-seeded) so a retried call across worker
/// restarts still collides on `(org_id, idempotency_key)`.
fn idempotency_key(
    thread: ThreadId,
    root: crate::runtime::PromptRequestId,
    gated_tool: &ToolName,
    action: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(action.as_bytes());
    let digest = hasher.finalize();
    let hex = crate::hex::encode_32(&digest[..8]);
    format!(
        "apv:{}:{}:{}:{hex}",
        thread.as_uuid(),
        root.as_uuid(),
        gated_tool.as_str()
    )
}

#[async_trait]
impl Tool for AskApprovalTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: AskApprovalInput = serde_json::from_value(input)?;
        let out = self.handle(parsed, ctx).await?;
        Ok(serde_json::to_string(&out)?)
    }

    fn modes(&self) -> super::super::modes::RequestKindModes {
        // A consequential checkpoint belongs only to normal chat turns, not
        // background cognition (reflection/resolution).
        super::super::modes::RequestKindModes::NORMAL
    }

    fn concurrency_safe(&self) -> bool {
        false
    }
}
