//! Bounds for the outbound-delivery seam (CLAUDE.md §5 — everything has a limit).

use std::time::Duration;

/// Upper bound on how many per-surface routers the composite fans out to.
///
/// One per chat surface (Lark, Discord, Slack, …) plus headroom; asserted on
/// construction so a wiring bug that pushes a router in a loop fails loudly
/// instead of growing an unbounded fan-out.
pub const MAX_OUTBOUND_ROUTERS: usize = 8;

/// Wall-clock cap on a single platform `ensure_delivery`.
///
/// Arm 2/3 perform a network post (DM create) and a few DB reads; a stuck
/// surface must not wedge the scheduler's fire loop or a tool call. On timeout
/// the delivery is dropped (best-effort) and logged — the message still lands on
/// the web feed.
pub const OUTBOUND_ENSURE_TIMEOUT: Duration = Duration::from_secs(10);
