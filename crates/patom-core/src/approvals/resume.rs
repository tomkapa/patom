//! Resume-via-fresh-trigger (issue #200).
//!
//! The worker is run-to-completion with no wait state, so a decided approval
//! does not "unblock" a suspended turn — instead it seeds the decision as an
//! owner-private `SystemNote` and enqueues a *fresh* `Normal` trigger that
//! REUSES the original DAG root (preserving turn budget + lineage). The agent
//! resumes run-to-completion with "APPROVAL approved/denied for: …" in context
//! and re-attempts the gated tool, which the hard gate now allows. Mirrors
//! `scheduler.rs`'s fire path.
//!
//! Double-click idempotency is belt + suspenders: the store's atomic `decide`
//! flips the row at most once, and `enqueue_trigger` is idempotent on
//! `(org_id, "apv-resume-{id}")`, so even two simultaneous decisions enqueue one
//! resume.

use std::sync::Arc;

use tracing::{info, warn};

use crate::auth::Caller;
use crate::colleagues::SharedColleagueStore;
use crate::provider::{ChatMessage, UserContent};
use crate::runtime::{
    IdempotencyKey, NewTrigger, PromptError, RequestKindPayload, SharedDagBudget, SharedPromptQueue,
};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore};

use super::error::ApprovalError;
use super::types::{ApprovalRecord, ApprovalStatus};

/// Seeds a decision note and wakes the requesting agent. Holds the same
/// collaborators `send_message` / the scheduler use.
#[derive(Clone)]
pub struct ApprovalResumer {
    threads: SharedThreadStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    colleagues: SharedColleagueStore,
}

impl std::fmt::Debug for ApprovalResumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalResumer").finish_non_exhaustive()
    }
}

impl ApprovalResumer {
    #[must_use]
    pub fn new(
        threads: SharedThreadStore,
        queue: SharedPromptQueue,
        dag: SharedDagBudget,
        colleagues: SharedColleagueStore,
    ) -> Self {
        Self {
            threads,
            queue,
            dag,
            colleagues,
        }
    }

    /// Resume the requesting agent after `record` was decided. Idempotent: safe
    /// to call again for the same approval (the trigger key dedupes).
    pub async fn resume(&self, record: &ApprovalRecord) -> Result<(), ApprovalError> {
        let decided_by = record.decided_by_colleague.ok_or_else(|| {
            ApprovalError::Backend("resume called on a non-decided approval".into())
        })?;
        let label = match record.status {
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Pending | ApprovalStatus::Expired => {
                return Err(ApprovalError::Backend(
                    "resume called on an unresolved approval".into(),
                ));
            }
        };

        // The approver becomes the principal driving the continuation: they are
        // an org member (RLS), and the seed + trigger carry their identity.
        let decider = self
            .colleagues
            .read(decided_by)
            .await
            .map_err(|e| ApprovalError::Backend(format!("resume: read decider: {e}")))?;
        let acting_user = decider.user_id().ok_or_else(|| {
            ApprovalError::Backend("resume: decider colleague has no user".into())
        })?;
        let decider_name = decider.display_name().as_str().to_owned();
        let caller = Caller::new(acting_user, record.org_id);

        let state = self
            .threads
            .resolve_participation(&caller, record.thread_id, record.requesting_agent_id)
            .await
            .map_err(|e| ApprovalError::Backend(format!("resume: participation: {e}")))?;

        let body = format!(
            "APPROVAL {label} for: {action}. Decided by {decider_name}.",
            action = record.action_summary.as_str()
        );
        let note_key = IdempotencyKey::try_from(format!("apv-note-{}", record.id))?;
        let note = self
            .threads
            .append(
                &caller,
                record.thread_id,
                NewMessage {
                    kind: MessageKind::SystemNote,
                    sender: None,
                    owner_agent_id: Some(record.requesting_agent_id),
                    receiver: None,
                    body: ChatMessage::User(vec![UserContent::Text(body)]),
                    request_id: Some(record.root_request_id),
                    idempotency_key: Some(note_key),
                },
            )
            .await
            .map_err(|e| ApprovalError::Backend(format!("resume: append note: {e}")))?;

        // Best-effort budget bump (mirrors send_message). On exhaustion the
        // worker rejects the resumed turn cleanly; we still enqueue so the
        // decision is not silently lost.
        if let Err(e) = self
            .dag
            .bump_or_fail_for_user(acting_user, record.root_request_id)
            .await
        {
            match e {
                PromptError::DagBudgetExceeded { .. } => {
                    warn!(patom.dag.root = %record.root_request_id, "approval.resume.dag_exceeded");
                }
                other => {
                    warn!(error = %other, patom.dag.root = %record.root_request_id, "approval.resume.dag_bump_failed");
                }
            }
        }

        let key = IdempotencyKey::try_from(format!("apv-resume-{}", record.id))?;
        let request_id = self
            .queue
            .enqueue_trigger(NewTrigger {
                org_id: record.org_id,
                acting_user_id: acting_user,
                thread_id: Some(record.thread_id),
                state_id: Some(state),
                background_turn_id: None,
                sender_colleague_id: decided_by,
                receiver_agent_id: record.requesting_agent_id,
                root_request_id: Some(record.root_request_id),
                trigger_message_id: Some(note),
                idempotency_key: key,
                kind_payload: RequestKindPayload::Normal {},
            })
            .await
            .map_err(|e| ApprovalError::Backend(format!("resume: enqueue: {e}")))?;

        info!(
            patom.approval.id = %record.id,
            patom.agent.id = %record.requesting_agent_id,
            patom.thread.id = %record.thread_id,
            patom.request.id = %request_id,
            approval.decision = label,
            "approval.resumed",
        );
        Ok(())
    }
}

/// Cheap-clone alias.
pub type SharedApprovalResumer = Arc<ApprovalResumer>;
