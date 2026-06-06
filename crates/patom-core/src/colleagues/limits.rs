//! Sizing constants for the colleagues subsystem (CLAUDE.md §5).
//!
//! Every bound lives here, named and doc-commented with *why this number*, so
//! the magic-number ban holds and the typed caps cannot drift from the DB
//! column `CHECK`s they mirror.

use std::time::Duration;

/// Max bytes of a colleague's rendered display name.
///
/// Mirrors the `users.display_name` storage cap (200 bytes). A colleague's
/// display name is resolved live from `agents.name` (≤ 64) or
/// `users.display_name` (≤ 200), so this is the larger of the two sources.
pub const COLLEAGUE_NAME_MAX_LEN: usize = 200;

/// Hard cap on rows a single roster fetch pulls from `colleagues` (§5).
///
/// The roster render (Stage 6) degrades to a `search` affordance above its own
/// inline cap, so this LIMIT just bounds the wire transfer if an org's
/// directory grows large. Sized generously — humans + agents per org.
pub const COLLEAGUE_ROSTER_FETCH_MAX: i64 = 1024;

/// Number of per-org rosters the [`super::ColleagueRosterCache`] holds.
///
/// Keyed by `OrgId` (one roster per org), so this bounds how many distinct
/// orgs stay hot at once. Matches [`crate::agents`]'s cache cap.
pub const COLLEAGUE_ROSTER_CACHE_CAP: usize = 256;

/// Liveness window for a cached roster — same as the agent caches so a
/// membership change or rename becomes visible within one window.
pub const COLLEAGUE_ROSTER_CACHE_TTL: Duration = Duration::from_mins(1);
