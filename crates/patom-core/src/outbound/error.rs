//! One error type for the outbound-delivery seam (CLAUDE.md §12).
//!
//! Platform routers map their own `DiscordError` / `LarkError` to
//! [`OutboundError::Backend`] at the edge, so the `outbound` module never
//! depends on a platform's error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OutboundError {
    /// A platform router failed to resolve a binding or attach its pump. The
    /// string is the platform error, flattened — the composite logs and
    /// continues (delivery is best-effort).
    #[error("outbound backend error: {0}")]
    Backend(String),

    /// A platform `ensure_delivery` exceeded [`super::limits::OUTBOUND_ENSURE_TIMEOUT`].
    #[error("outbound delivery timed out")]
    Timeout,
}
