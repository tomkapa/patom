//! Per-session cache of composed memory sections, backed by
//! [`BoundedTtlCache`]. Cheap-clone — sharing the cache between
//! subsystems does not need an external `Arc<...>` wrapper.
//!
//! Divergence from `doc/memory.md` §1.3: the doc reads "assembled at
//! session start and frozen for the session's lifetime". We ship a
//! process-local TTL cache instead — composition is lazy on the first
//! turn that needs it. A session that outlives
//! [`crate::memory::SESSION_MEMORY_CACHE_TTL_SECS`] or survives a process
//! restart will recompose and pick up any writes that landed in between.
//! Revisit by promoting to a `session_memory_snapshots` table if drift
//! across long-lived sessions becomes a correctness issue.

use std::sync::Arc;
use std::time::Duration;

use crate::agents::AgentId;
use crate::cache::BoundedTtlCache;
use crate::clock::SharedClock;
use crate::session::SessionId;

use super::composer::MemorySection;

/// Per (session, agent) cache of composed sections. Cheap-clone — the
/// inner [`BoundedTtlCache`] is itself an `Arc`, so cloning shares the
/// underlying state.
#[derive(Debug, Clone)]
pub struct SessionMemoryCache {
    inner: BoundedTtlCache<(SessionId, AgentId), Arc<MemorySection>>,
}

impl SessionMemoryCache {
    #[must_use]
    pub fn new(cap: usize, ttl: Duration, clock: SharedClock) -> Self {
        Self {
            inner: BoundedTtlCache::new(cap, ttl, clock, "SessionMemoryCache"),
        }
    }

    /// Return the cached section for `(session, agent)`, calling
    /// `compose` to produce one on miss or expiry. The lock is released
    /// before `compose` runs so a slow store does not block other
    /// workers.
    pub async fn get_or_load<F, Fut, E>(
        &self,
        session: SessionId,
        agent: AgentId,
        compose: F,
    ) -> Result<Arc<MemorySection>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<MemorySection, E>>,
    {
        self.inner
            .get_or_load((session, agent), || async move {
                let composed = compose().await?;
                Ok::<_, E>(Arc::new(composed))
            })
            .await
    }
}
