//! Approval subsystem caps. CLAUDE.md §5 — every value documented with *why
//! this number*. The caps gate the tool boundary, the store, and the expiry
//! sweep from one source of truth.

use std::time::Duration;

/// Maximum bytes of an [`ActionSummary`](super::types::ActionSummary).
///
/// Mirrors the column `CHECK (octet_length(action_summary) BETWEEN 1 AND 2048)`
/// in migration 90. Sized for a one- or two-sentence description of the gated
/// action ("Refund $240 to customer #8812 for order O-5567") — long enough to
/// be unambiguous to the approver, short enough not to bloat the card.
pub const APPROVAL_SUMMARY_MAX: usize = 2048;

/// Hard cap on the size of a `OneOf` approver whitelist.
///
/// Bounds the child-table fan-out per approval and the work the authorize check
/// does on a click. An approval routed to "one of" more than this many people
/// is almost certainly mis-scoped; pick a channel target instead.
pub const MAX_APPROVERS: usize = 16;

/// Default time-to-live for a pending approval when the caller omits one.
///
/// One business day: long enough that an approver who steps away for a meeting
/// (or overnight) can still act, short enough that a stale request does not
/// linger as a clickable button after the context has moved on.
// `Duration::from_hours`/`from_days` are not yet stable in `const` context
// (the `duration_constructors` feature), so a `const` TTL must use `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
pub const APPROVAL_DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard ceiling on a pending approval's TTL; the tool clamps any larger request.
///
/// A week bounds how long a button stays live (and how long the expiry sweep
/// must keep scanning a row) while covering "I'll get to it after the weekend".
#[allow(clippy::duration_suboptimal_units)]
pub const APPROVAL_MAX_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Per-tick batch limit on the expiry sweep's claim query.
///
/// Caps the rows one sweep tick flips to `expired` so a backlog (sweeper paused
/// during an outage) drains over several bounded ticks instead of one unbounded
/// statement holding a long write lock.
pub const APPROVAL_SWEEP_BATCH: i64 = 256;
