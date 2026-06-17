//! The decision seam shared by every approval surface.
//!
//! Discord (Gateway `INTERACTION_CREATE`), Lark (the `card.action.trigger` HTTP
//! route), and the web UI all resolve a click the same way: authorize + atomic
//! `decide`, and on a *newly* recorded `Approved`/`Denied`, seed the decision and
//! enqueue the resume trigger. Concentrating that here keeps the platform layers
//! thin and the idempotency guarantee in one place — a double-click flips the row
//! once (`decide`) and enqueues one resume (`enqueue_trigger` keyed on
//! `apv-resume-{id}`), so `AlreadyDecided` simply re-renders without re-resuming.

use std::sync::Arc;

use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;

use super::error::ApprovalError;
use super::resume::SharedApprovalResumer;
use super::store::{DecideOutcome, SharedApprovalStore};
use super::types::{ApprovalId, Decision};

/// Authorizes + decides an approval, then resumes the requesting agent on a
/// newly-recorded decision. Held by the Discord bridge and the Lark card route.
#[derive(Clone)]
pub struct ApprovalDecider {
    store: SharedApprovalStore,
    resumer: SharedApprovalResumer,
    clock: SharedClock,
}

impl std::fmt::Debug for ApprovalDecider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalDecider").finish_non_exhaustive()
    }
}

impl ApprovalDecider {
    #[must_use]
    pub fn new(
        store: SharedApprovalStore,
        resumer: SharedApprovalResumer,
        clock: SharedClock,
    ) -> Self {
        Self {
            store,
            resumer,
            clock,
        }
    }

    /// Resolve `approval_id` for `clicker`. Returns the outcome so the caller can
    /// render the resolved view. Resume fires only for `Decided` (the first,
    /// authorized flip); `AlreadyDecided` is idempotent and does not re-resume.
    pub async fn decide(
        &self,
        org_id: OrgId,
        approval_id: ApprovalId,
        decision: Decision,
        clicker: ColleagueId,
    ) -> Result<DecideOutcome, ApprovalError> {
        let outcome = self
            .store
            .decide(org_id, approval_id, decision, clicker, self.clock.now_utc())
            .await?;
        if let DecideOutcome::Decided(record) = &outcome {
            self.resumer.resume(record).await?;
        }
        Ok(outcome)
    }
}

/// Cheap-clone alias.
pub type SharedApprovalDecider = Arc<ApprovalDecider>;
