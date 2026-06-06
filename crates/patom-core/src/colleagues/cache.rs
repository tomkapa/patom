//! Bounded TTL cache for the per-org colleague roster.
//!
//! The system-prompt roster block (Stage 6) hits this on every turn; a miss or
//! expiry falls through to [`ColleagueStore::list_for_org`]. Keyed by [`OrgId`]
//! because the roster is org-wide — every agent in an org sees the same list.
//! Backed by the generic [`BoundedTtlCache`] so the eviction / TTL machinery
//! stays in one place (mirrors [`crate::agents::AgentNamesCache`]).

use std::sync::Arc;
use std::time::Duration;

use crate::auth::OrgId;
use crate::cache::BoundedTtlCache;
use crate::clock::SharedClock;

use super::error::ColleagueError;
use super::store::SharedColleagueStore;
use super::types::ColleagueRef;

/// Bounded TTL cache keyed by [`OrgId`]. Cheap-clone — the inner
/// [`BoundedTtlCache`] is itself an `Arc`, so cloning shares state.
#[derive(Debug, Clone)]
pub struct ColleagueRosterCache {
    inner: BoundedTtlCache<OrgId, Arc<[ColleagueRef]>>,
}

impl ColleagueRosterCache {
    #[must_use]
    pub fn new(cap: usize, ttl: Duration, clock: SharedClock) -> Self {
        Self {
            inner: BoundedTtlCache::new(cap, ttl, clock, "ColleagueRosterCache"),
        }
    }

    /// Return the cached roster for `org_id`, refreshing from `store` on miss
    /// or expiry. The lock is released before the store call so a slow database
    /// does not block other workers.
    pub async fn get_or_load(
        &self,
        org_id: OrgId,
        store: &SharedColleagueStore,
    ) -> Result<Arc<[ColleagueRef]>, ColleagueError> {
        self.inner
            .get_or_load(org_id, || async move {
                let roster = store.list_for_org(org_id).await?;
                Ok::<_, ColleagueError>(Arc::from(roster))
            })
            .await
    }
}
