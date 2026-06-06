//! Background coordinator for [`McpRegistry::refresh`].
//!
//! CRUD handlers must not block on (or fire-and-forget through `tokio::spawn`) the
//! per-server connect + list_tools work. They signal a single coordinator task via a
//! cheap clone-able trigger; the task coalesces bursts and runs one refresh at a time.
//! The owned `JoinHandle` lives in the [`Server`](crate::app::Server) so graceful
//! shutdown waits on it (CLAUDE.md §7 — no floating tasks).

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;

use super::registry::McpRegistry;

/// Buffer = 1: a queued signal already subsumes any later one. `try_send` on a full
/// buffer silently drops — exactly the coalescing we want.
const REFRESH_QUEUE_CAP: usize = 1;

/// Send-half of the refresh signal. Cheap to clone; lives in `AppState` so every CRUD
/// handler can ask for a refresh without owning the coordinator.
#[derive(Clone, Debug)]
pub struct McpRefreshTrigger {
    tx: mpsc::Sender<()>,
}

impl McpRefreshTrigger {
    /// Non-blocking. If a refresh is already pending, drop this one — the queued signal
    /// will see our write once it runs.
    pub fn request(&self) {
        match self.tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                warn!("mcp.refresh.trigger.closed");
            }
        }
    }
}

/// Owned-handle wrapper around the coordinator task. `shutdown().await` (or simply
/// dropping it) cancels the task and joins it.
pub struct McpRefresher {
    shutdown: DropGuard,
    handle: JoinHandle<()>,
}

impl std::fmt::Debug for McpRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRefresher").finish_non_exhaustive()
    }
}

impl McpRefresher {
    /// Spawn the single coordinator task. Returns `self` (hand to the `Server` for
    /// graceful shutdown) and a [`McpRefreshTrigger`] (clone into `AppState`).
    #[must_use]
    pub fn spawn(registry: McpRegistry) -> (Self, McpRefreshTrigger) {
        let (tx, mut rx) = mpsc::channel::<()>(REFRESH_QUEUE_CAP);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => return,
                    msg = rx.recv() => {
                        if msg.is_none() {
                            return;
                        }
                        // Drain any extras that landed while we were waiting — they're
                        // covered by the refresh we're about to run.
                        while rx.try_recv().is_ok() {}
                        if let Err(e) = registry.refresh().await {
                            warn!(error = %e, "mcp.registry.refresh_failed");
                        }
                    }
                }
            }
        });
        (
            Self {
                shutdown: cancel.drop_guard(),
                handle,
            },
            McpRefreshTrigger { tx },
        )
    }

    /// Cancel the coordinator and await its termination.
    pub async fn shutdown(self) {
        drop(self.shutdown);
        if let Err(e) = self.handle.await {
            warn!(error = %e, "mcp.refresher.join.error");
        }
    }
}
