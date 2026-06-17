//! Outbound delivery seam (issue #178).
//!
//! Decouples "ensure this thread's chunks reach its external surface" from the
//! inbound bridge that historically owned it. Any trigger source — the
//! scheduler, an agent→agent hand-off, or a proactive `send_message` to a
//! channel/DM — calls [`OutboundRouter::ensure_delivery`] for the thread it
//! produced, and the matching per-platform router (in `discord::outbound_router`
//! / `lark::outbound_router`) resolves the binding and attaches the surface pump.

pub mod error;
pub mod limits;
pub mod router;

pub use error::OutboundError;
pub use router::{
    CompositeOutboundRouter, DeferredOutboundRouter, NoopOutboundRouter, OutboundRouter,
    SharedOutboundRouter,
};
