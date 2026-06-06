//! Bounds for the workspace-settings surface (CLAUDE.md §5).

use std::time::Duration;

/// Single-use invite link TTL. Mirrors the user-visible "Links expire
/// in 7 days" copy in the design's invite modal.
pub const INVITE_TTL: Duration = Duration::from_hours(24 * 7);

/// Maximum emails accepted by `POST /me/org/invites` in one call.
pub const MAX_INVITE_BATCH: usize = 25;

/// Hard ceiling on `?per_page=` for the members listing.
///
/// Typed as `u32` to match the wire-side `?per_page=` query
/// parameter — the only callers compare or clamp against another
/// `u32`, so a `usize` would just sprout `try_from` boilerplate at
/// every seam.
pub const MAX_MEMBERS_PER_PAGE: u32 = 50;

/// Wall-clock budget for one outbound invite email (connect + AUTH +
/// send) before the [`crate::orgs::SmtpMailer`] gives up (CLAUDE.md §5 —
/// every I/O `await` is bounded). Invite delivery is fire-and-forget and
/// off the request's critical path, so the bound is generous: a slow but
/// eventually-succeeding relay should still deliver rather than time out,
/// while a black-holed connection must not pin a task indefinitely. 15s
/// comfortably covers a STARTTLS handshake to a healthy relay.
pub(super) const EMAIL_SEND_TIMEOUT: Duration = Duration::from_secs(15);
