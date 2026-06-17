//! The approval store seam. Two access classes (CLAUDE.md §10/§Tenancy):
//!
//! - **Tenant-side writes** (`create`, `attach_message`) run from the
//!   `ask_approval` tool, which has a Patom principal (`Caller`) — they open a
//!   `begin_as_user` tx so RLS WITH CHECK fires against the acting member.
//! - **Webhook-side reads/decide/sweep** run from the Discord Gateway / Lark
//!   card callback, which has NO Patom session principal — they run privileged
//!   (RLS bypass) with `org_id` taken from the verified app, like the directory
//!   shadow mints. The unguessable `root_request_id` / `approval_id` is the
//!   capability; `org_id` scoping is the tenancy guard.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId};
use crate::colleagues::ColleagueId;
use crate::runtime::PromptRequestId;
use crate::threads::ThreadId;
use crate::types::ToolName;

use super::error::ApprovalError;
use super::types::{
    ActionSummary, ApprovalId, ApprovalRecord, ApproverPolicy, Decision, PlatformMessageId,
    PlatformTarget,
};

/// Everything needed to insert one `pending_approval` (+ its `OneOf` child rows).
#[derive(Debug, Clone)]
pub struct NewApproval {
    pub id: ApprovalId,
    pub thread_id: ThreadId,
    pub requesting_agent_id: AgentId,
    pub requesting_colleague_id: ColleagueId,
    pub root_request_id: PromptRequestId,
    pub action_summary: ActionSummary,
    pub gated_tool: ToolName,
    pub approvers: ApproverPolicy,
    pub target: PlatformTarget,
    pub idempotency_key: String,
    pub expires_at: DateTime<Utc>,
}

/// Result of [`ApprovalStore::create`].
///
/// `Existing` is the idempotent path — a repeated `ask_approval` in one DAG hit
/// the `(org_id, idempotency_key)` conflict and we return the row already there
/// rather than a duplicate.
#[derive(Debug, Clone)]
pub enum CreateOutcome {
    Created(ApprovalRecord),
    Existing(ApprovalRecord),
}

impl CreateOutcome {
    #[must_use]
    pub fn record(&self) -> &ApprovalRecord {
        match self {
            Self::Created(r) | Self::Existing(r) => r,
        }
    }
}

/// Result of [`ApprovalStore::decide`].
///
/// The atomic `UPDATE ... WHERE status='pending'` flips the row at most once; a
/// double-click sees the already-resolved row via `AlreadyDecided` and the
/// caller just re-renders the resolved view (idempotent).
#[derive(Debug, Clone)]
pub enum DecideOutcome {
    Decided(ApprovalRecord),
    AlreadyDecided(ApprovalRecord),
}

#[async_trait]
pub trait ApprovalStore: std::fmt::Debug + Send + Sync {
    /// Insert a pending approval under the acting member, deduped on
    /// `(org_id, idempotency_key)`.
    async fn create(
        &self,
        caller: &Caller,
        new: NewApproval,
    ) -> Result<CreateOutcome, ApprovalError>;

    /// Record the posted prompt's platform message id so the resolve path can
    /// edit it. Tenant-side (the tool just posted).
    async fn attach_message(
        &self,
        caller: &Caller,
        id: ApprovalId,
        message_id: PlatformMessageId,
    ) -> Result<(), ApprovalError>;

    /// Privileged read by id, org-scoped. Used by the webhook intake.
    async fn read(&self, org_id: OrgId, id: ApprovalId) -> Result<ApprovalRecord, ApprovalError>;

    /// Atomically resolve a pending approval, authorizing `clicker` against the
    /// row's policy first. Privileged (webhook path). Returns the resolved row;
    /// a second click returns [`DecideOutcome::AlreadyDecided`].
    async fn decide(
        &self,
        org_id: OrgId,
        id: ApprovalId,
        decision: Decision,
        clicker: ColleagueId,
        now: DateTime<Utc>,
    ) -> Result<DecideOutcome, ApprovalError>;

    /// Flip up to a bounded batch of pending rows whose TTL elapsed to
    /// `expired`. Privileged sweep. Returns the count flipped.
    async fn expire_due(&self, now: DateTime<Utc>) -> Result<u64, ApprovalError>;

    /// The hard gate's query: does an `approved` row exist for this DAG root and
    /// tool? Privileged read keyed by the unguessable `root_request_id`.
    async fn has_approved_for_dag(
        &self,
        org_id: OrgId,
        root: PromptRequestId,
        tool: &ToolName,
    ) -> Result<bool, ApprovalError>;
}

/// Cheap-clone alias threaded through the tool, the gate, and the webhook intake.
pub type SharedApprovalStore = std::sync::Arc<dyn ApprovalStore>;
