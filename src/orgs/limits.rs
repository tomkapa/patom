//! Bounds for the workspace-settings surface (CLAUDE.md §5).

use std::time::Duration;

/// Single-use invite link TTL. Mirrors the user-visible "Links expire
/// in 7 days" copy in the design's invite modal.
pub const INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Maximum emails accepted by `POST /me/org/invites` in one call.
pub const MAX_INVITE_BATCH: usize = 25;

/// Hard ceiling on `?per_page=` for the members listing.
///
/// Typed as `u32` to match the wire-side `?per_page=` query
/// parameter — the only callers compare or clamp against another
/// `u32`, so a `usize` would just sprout `try_from` boilerplate at
/// every seam.
pub const MAX_MEMBERS_PER_PAGE: u32 = 50;
