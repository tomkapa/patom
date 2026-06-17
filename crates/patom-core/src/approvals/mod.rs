//! Human-in-the-loop approval gating (issue #200).
//!
//! An agent calls the `ask_approval` tool before a consequential, approval-gated
//! action; the tool records a pending row, posts an interactive prompt (Discord
//! buttons / Lark card), and ends the turn (the worker is run-to-completion with
//! no wait state). A later human click authorizes + atomically decides the row
//! and enqueues a *fresh* trigger seeding the decision; the agent resumes and
//! re-attempts the gated tool, which a hard pre-execution [`gate`] now allows
//! because a matching `approved` decision exists for the DAG.
//!
//! Module map: [`types`] (newtypes/enums), [`error`] (one error type +
//! `IntoResponse`), [`store`] (the approval store seam) + [`pg_store`], [`config`]
//! (per-agent gated-tool config) reusing the same pg impl, [`gate`] (the hard
//! pre-execution gate), [`resume`] (decision → fresh trigger), [`limits`].

pub mod config;
pub mod decision;
pub mod error;
pub mod gate;
pub mod limits;
pub mod pg_store;
pub mod resume;
pub mod store;
pub mod types;

pub use config::{GatedToolStore, SharedGatedToolStore};
pub use decision::{ApprovalDecider, SharedApprovalDecider};
pub use error::ApprovalError;
pub use gate::{ApprovalGate, GateOutcome, HardApprovalGate, SharedApprovalGate};
pub use pg_store::PgApprovalStore;
pub use resume::{ApprovalResumer, SharedApprovalResumer};
pub use store::{ApprovalStore, CreateOutcome, DecideOutcome, NewApproval, SharedApprovalStore};
pub use types::{
    ActionSummary, ApprovalId, ApprovalRecord, ApprovalStatus, ApproverKind, ApproverPolicy,
    Decision, Platform, PlatformMessageId, PlatformTarget, policy_allows,
};
