// Lint conflict: `unreachable_pub` and `clippy::redundant_pub_crate` are
// mutually exclusive for cross-module-reused items inside a `pub(crate)`
// module. The struct is `pub(crate)` because the memory subsystem
// (`memory::reflection_scheduler` / `memory::librarian`) reuses it; that
// makes `pub` "unreachable" outside the crate but `redundant_pub_crate`
// considers `pub(crate)` redundant inside a non-`pub` module. Choose
// the lint that better describes intent and silence the other.
#![allow(clippy::redundant_pub_crate)]

//! Shared background-task scaffolding for the schedulers in this crate.
//!
//! [`ReflectionScheduler`](crate::memory::ReflectionScheduler),
//! [`LibrarianScheduler`](crate::memory::LibrarianScheduler), and
//! [`ScheduledTaskScheduler`](super::scheduler::ScheduledTaskScheduler) all
//! follow the same shape: a spawned tokio task that ticks on a fixed cadence,
//! exits on parent-token or owned-`DropGuard` cancellation, and forwards
//! every tick error to a `tracing::warn!`. [`ScheduledTask`] owns that
//! wiring; each scheduler reduces to one `tick()` closure and an event
//! name used for the warn log.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;

/// Cancellable background task that ticks on a fixed cadence.
///
/// Two cancellation paths fire together: the owned [`DropGuard`] cancels
/// an internal child token on drop / explicit [`Self::shutdown`], and the
/// optional parent token wires the loop into a process-wide Ctrl+C.
pub(crate) struct ScheduledTask {
    shutdown: DropGuard,
    handle: JoinHandle<()>,
    label: &'static str,
}

impl fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl ScheduledTask {
    /// Spawn `tick` to run every `interval`. `label` rides on the
    /// per-iteration warn log (`{label}.tick.error`) so a wedged scheduler
    /// is identifiable in tracing.
    pub(crate) fn spawn<F, Fut, E>(
        label: &'static str,
        interval: Duration,
        parent: Option<CancellationToken>,
        mut tick: F,
    ) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send,
        E: fmt::Display + Send,
    {
        let owned = CancellationToken::new();
        let local = owned.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = local.cancelled() => return,
                    () = parent_cancelled(parent.as_ref()) => return,
                    () = tokio::time::sleep(interval) => {},
                }
                if let Err(e) = tick().await {
                    warn!(error = %e, scheduler = label, "scheduled_task.tick.error");
                }
            }
        });
        Self {
            shutdown: owned.drop_guard(),
            handle,
            label,
        }
    }

    /// Cancel and join. Idempotent.
    pub(crate) async fn shutdown(self) {
        drop(self.shutdown);
        if let Err(e) = self.handle.await {
            warn!(error = %e, scheduler = self.label, "scheduled_task.join.error");
        }
    }
}

/// Resolves only when `parent` is `Some` and cancelled. `None` parents
/// resolve never, so the loop only watches the local token.
async fn parent_cancelled(parent: Option<&CancellationToken>) {
    match parent {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}
