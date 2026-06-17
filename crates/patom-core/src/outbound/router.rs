//! The `OutboundRouter` seam: ensure a thread's feed reaches the external
//! surface it belongs to, from any caller holding only `(org_id, thread_id)`.
//!
//! The per-platform stream pumps stay the low-level attach; this trait is the
//! higher-level bind+resolve+attach layer the scheduler, the inbound bridges,
//! and `send_message`/`schedule_task` all call. A [`NoopOutboundRouter`] is the
//! default when no chat surface is configured, so callers always hold a
//! `SharedOutboundRouter` and never branch on `Option` (CLAUDE.md §4 — push the
//! env-gating up to the composition root).

use std::fmt;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tracing::warn;

use crate::auth::OrgId;
use crate::threads::ThreadId;

use super::error::OutboundError;
use super::limits::MAX_OUTBOUND_ROUTERS;

#[async_trait]
pub trait OutboundRouter: fmt::Debug + Send + Sync {
    /// Ensure a per-surface pump is attached for `thread_id` so this thread's
    /// feed chunks reach the external surface it belongs to. Idempotent (the
    /// pumps are keyed by thread id) and a no-op when the thread is web-origin
    /// or belongs to a different surface. Best-effort: a returned error is
    /// logged by the composite, never propagated to the caller's hot path.
    async fn ensure_delivery(
        &self,
        org_id: OrgId,
        thread_id: ThreadId,
    ) -> Result<(), OutboundError>;
}

pub type SharedOutboundRouter = Arc<dyn OutboundRouter>;

/// A router whose real implementation is installed after construction.
///
/// The composition root builds `send_message`'s tool (and so this router) before
/// the chat-surface pumps exist — but the composite that routes to them can only
/// be built once the pumps are up. This slot bridges the gap: hand it to the tool
/// early, [`set`](Self::set) the composite once composed. Until then,
/// `ensure_delivery` is a no-op (the thread still reaches the web feed).
#[derive(Debug, Default)]
pub struct DeferredOutboundRouter {
    inner: OnceLock<SharedOutboundRouter>,
}

impl DeferredOutboundRouter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    /// Install the composed router. Call exactly once at the composition root;
    /// a second call is ignored and logged (the first install wins).
    pub fn set(&self, router: SharedOutboundRouter) {
        if self.inner.set(router).is_err() {
            warn!("outbound.deferred.set_after_set");
        }
    }
}

#[async_trait]
impl OutboundRouter for DeferredOutboundRouter {
    async fn ensure_delivery(
        &self,
        org_id: OrgId,
        thread_id: ThreadId,
    ) -> Result<(), OutboundError> {
        match self.inner.get() {
            Some(router) => router.ensure_delivery(org_id, thread_id).await,
            None => Ok(()), // Surfaces not composed yet — web-only.
        }
    }
}

/// The no-surface default. Always a no-op.
#[derive(Debug, Default)]
pub struct NoopOutboundRouter;

#[async_trait]
impl OutboundRouter for NoopOutboundRouter {
    async fn ensure_delivery(
        &self,
        _org_id: OrgId,
        _thread_id: ThreadId,
    ) -> Result<(), OutboundError> {
        Ok(())
    }
}

/// Fans `ensure_delivery` out to every enabled per-platform router.
///
/// Each router self-skips a thread that is not its surface, so the composite
/// calls all of them; a single router's failure is logged and does not abort the
/// others.
#[derive(Debug)]
pub struct CompositeOutboundRouter {
    routers: Vec<SharedOutboundRouter>,
}

impl CompositeOutboundRouter {
    /// Build the composite. Asserts the fan-out is bounded (CLAUDE.md §5).
    #[must_use]
    pub fn new(routers: Vec<SharedOutboundRouter>) -> Self {
        assert!(
            routers.len() <= MAX_OUTBOUND_ROUTERS,
            "outbound router fan-out exceeds bound"
        );
        Self { routers }
    }
}

#[async_trait]
impl OutboundRouter for CompositeOutboundRouter {
    async fn ensure_delivery(
        &self,
        org_id: OrgId,
        thread_id: ThreadId,
    ) -> Result<(), OutboundError> {
        // The fan-out bound is asserted once in `new`; `routers` is immutable
        // after construction, so no per-call re-check is needed.
        for router in &self.routers {
            if let Err(e) = router.ensure_delivery(org_id, thread_id).await {
                warn!(
                    error = ?e,
                    patom.thread.id = %thread_id,
                    "outbound.ensure_delivery.router_failed"
                );
            }
        }
        Ok(())
    }
}
