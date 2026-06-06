//! Bounded, TTL'd cache of `(user_id, org_id) → role` lookups for
//! [`super::auth_layer::require_principal`].
//!
//! Without this, every authenticated request issues 3 DB round-trips
//! (`begin_privileged` → `SELECT role` → `COMMIT`) before the handler
//! sees a `Principal`. Caching role for a short window (default
//! `MEMBERSHIP_TTL_SECS`) collapses the hot path to zero round-trips
//! when the same `(user, org)` is hitting the API repeatedly, which is
//! the common case during interactive use.
//!
//! Caveats:
//! - Stale TTL window for role *demotions* — a user kept at `member`
//!   after being downgraded from `owner` will see their old role for up
//!   to one TTL. Role escalations always go via fresh login, which
//!   bypasses the cache entirely (new JWT → next request's lookup
//!   refreshes the entry).
//! - Negative results are NOT cached — a user newly invited to an org
//!   should be admitted on their next request, not wait out a TTL.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::auth::{OrgId, Role, UserId};
use crate::clock::SharedClock;

/// Default TTL: 30 seconds. Short enough that role *changes* propagate
/// within a quarter-minute, long enough to absorb interactive bursts
/// (the typical user clicks ~10× per 30s through the UI).
const MEMBERSHIP_TTL_SECS: u64 = 30;

/// Default cap. Bounds the cache so a flood of distinct callers can't
/// grow it unboundedly (CLAUDE.md §5). At ~64 bytes per entry this is
/// ~256 KiB — negligible.
const MEMBERSHIP_CAP: usize = 4096;

/// One cached membership lookup.
#[derive(Debug)]
struct CacheEntry {
    role: Role,
    expires_at: Instant,
}

#[derive(Debug)]
pub struct MembershipCache {
    inner: Mutex<HashMap<(UserId, OrgId), CacheEntry>>,
    cap: usize,
    ttl: Duration,
    clock: SharedClock,
}

impl MembershipCache {
    #[must_use]
    pub fn new(clock: SharedClock) -> Self {
        Self::with_params(
            clock,
            MEMBERSHIP_CAP,
            Duration::from_secs(MEMBERSHIP_TTL_SECS),
        )
    }

    #[must_use]
    pub fn with_params(clock: SharedClock, cap: usize, ttl: Duration) -> Self {
        assert!(cap > 0, "invariant: membership cache cap must be positive");
        Self {
            inner: Mutex::new(HashMap::with_capacity(cap)),
            cap,
            ttl,
            clock,
        }
    }

    /// Snapshot the cached role for `(user, org)` if it exists and is
    /// not expired. Holds the lock for the duration of the lookup only
    /// — never across an `await`.
    pub fn lookup(&self, user: UserId, org: OrgId) -> Option<Role> {
        let now = self.clock.now();
        let map = self
            .inner
            .lock()
            .expect("invariant: membership cache mutex never poisoned");
        map.get(&(user, org)).and_then(|entry| {
            if entry.expires_at > now {
                Some(entry.role)
            } else {
                None
            }
        })
    }

    /// Insert a fresh lookup. Bounded by `cap` — when the map is full
    /// we drop the entire contents rather than tracking an LRU; the
    /// next requests rebuild it. Simple and zero-dep (CLAUDE.md §8).
    pub fn insert(&self, user: UserId, org: OrgId, role: Role) {
        let expires_at = self.clock.now() + self.ttl;
        let mut map = self
            .inner
            .lock()
            .expect("invariant: membership cache mutex never poisoned");
        if map.len() >= self.cap {
            map.clear();
        }
        map.insert((user, org), CacheEntry { role, expires_at });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::auth::{OrgId, Role, UserId};
    use crate::clock::TestClock;

    use super::MembershipCache;

    #[test]
    fn hit_then_miss_after_ttl() {
        let clock = Arc::new(TestClock::new());
        let cache =
            MembershipCache::with_params(clock.clone(), 8, std::time::Duration::from_secs(5));
        let user = UserId::new();
        let org = OrgId::new();
        assert!(cache.lookup(user, org).is_none());
        cache.insert(user, org, Role::Owner);
        assert_eq!(cache.lookup(user, org), Some(Role::Owner));
        clock.advance(std::time::Duration::from_secs(6));
        assert!(
            cache.lookup(user, org).is_none(),
            "entry should be expired past TTL"
        );
    }

    #[test]
    fn cap_evicts_when_exceeded() {
        let clock = Arc::new(TestClock::new());
        let cache = MembershipCache::with_params(clock, 2, std::time::Duration::from_mins(1));
        let user = UserId::new();
        let org_a = OrgId::new();
        let org_b = OrgId::new();
        let org_c = OrgId::new();
        cache.insert(user, org_a, Role::Member);
        cache.insert(user, org_b, Role::Member);
        // Third insert tips us over the cap → the prior contents are
        // dropped; only the newest entry remains.
        cache.insert(user, org_c, Role::Owner);
        assert_eq!(cache.lookup(user, org_c), Some(Role::Owner));
        assert!(cache.lookup(user, org_a).is_none());
        assert!(cache.lookup(user, org_b).is_none());
    }
}
