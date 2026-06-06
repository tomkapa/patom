//! Bounded in-memory per-user rate limiter for the `test-connect` endpoint.
//!
//! Process-wide singleton. Each user gets one bucket that records the
//! timestamps of the last `CAP` calls; a new call is admitted iff fewer than
//! `CAP` of those timestamps fall inside a rolling minute. The bucket map is
//! itself capped at [`MCP_TEST_CONNECT_BUCKETS_MAX`] entries; when full, the
//! oldest-touched bucket is evicted so a flood of distinct user ids cannot
//! grow memory unboundedly (CLAUDE.md §5).
//!
//! Memory: each bucket is `CAP` × `Instant` (≈ 80 bytes) plus a "last touch"
//! `Instant`; at the eviction cap that's < 0.5 MiB. Trivial vs. pulling in a
//! leaky-bucket crate just for this endpoint (CLAUDE.md §8).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::auth::UserId;
use crate::clock::SharedClock;

use super::limits::{MCP_TEST_CONNECT_BUCKETS_MAX, MCP_TEST_CONNECT_PER_MIN};

const WINDOW: Duration = Duration::from_mins(1);

struct Bucket {
    /// Timestamps of recent admits, oldest first. Capped at
    /// `MCP_TEST_CONNECT_PER_MIN` entries by the admit loop.
    samples: VecDeque<Instant>,
    /// Last time this bucket was touched (admit attempted). Used to pick the
    /// LRU victim when the map exceeds `MCP_TEST_CONNECT_BUCKETS_MAX`.
    last_touched: Instant,
}

/// Process-wide rate limiter handle. Cheap to clone; the inner state is
/// shared via `Arc<Mutex<...>>`.
#[derive(Clone)]
pub struct TestConnectRateLimiter {
    inner: std::sync::Arc<Mutex<HashMap<UserId, Bucket>>>,
    clock: SharedClock,
}

impl std::fmt::Debug for TestConnectRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestConnectRateLimiter")
            .finish_non_exhaustive()
    }
}

impl TestConnectRateLimiter {
    #[must_use]
    pub fn new(clock: SharedClock) -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(HashMap::with_capacity(64))),
            clock,
        }
    }

    /// Try to admit one call for `user`. Returns `true` when admitted and the
    /// call should proceed; `false` means the per-minute cap has been hit and
    /// the handler must return 429.
    pub fn try_admit(&self, user: UserId) -> bool {
        let now = self.clock.now();
        let mut guard: MutexGuard<'_, HashMap<UserId, Bucket>> = self
            .inner
            .lock()
            .expect("invariant: rate limiter mutex poisoned");

        let cutoff = now.checked_sub(WINDOW);

        let bucket = guard.entry(user).or_insert_with(|| Bucket {
            samples: VecDeque::with_capacity(MCP_TEST_CONNECT_PER_MIN),
            last_touched: now,
        });
        bucket.last_touched = now;

        if let Some(cutoff) = cutoff {
            while bucket.samples.front().is_some_and(|t| *t < cutoff) {
                bucket.samples.pop_front();
            }
        }
        assert!(bucket.samples.len() <= MCP_TEST_CONNECT_PER_MIN);
        if bucket.samples.len() >= MCP_TEST_CONNECT_PER_MIN {
            return false;
        }
        bucket.samples.push_back(now);

        if guard.len() > MCP_TEST_CONNECT_BUCKETS_MAX {
            // LRU eviction: find the bucket with the oldest `last_touched`,
            // excluding the one we just touched. O(n) in `n = cap`; bounded.
            let victim = guard
                .iter()
                .filter(|(k, _)| **k != user)
                .min_by_key(|(_, b)| b.last_touched)
                .map(|(k, _)| *k);
            if let Some(v) = victim {
                guard.remove(&v);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::clock::Clock;

    #[derive(Debug)]
    struct FakeClock {
        inner: Mutex<Instant>,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(Instant::now()),
            })
        }

        fn advance(&self, by: Duration) {
            *self.inner.lock().expect("fake clock") += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.inner.lock().expect("fake clock")
        }
        fn now_wall(&self) -> std::time::SystemTime {
            std::time::SystemTime::now()
        }
    }

    #[test]
    fn admits_up_to_cap_then_rejects() {
        let clock = FakeClock::new();
        let rl = TestConnectRateLimiter::new(clock);
        let user = UserId::new();
        for _ in 0..MCP_TEST_CONNECT_PER_MIN {
            assert!(rl.try_admit(user));
        }
        assert!(!rl.try_admit(user));
    }

    #[test]
    fn window_slides() {
        let clock = FakeClock::new();
        let rl = TestConnectRateLimiter::new(clock.clone());
        let user = UserId::new();
        for _ in 0..MCP_TEST_CONNECT_PER_MIN {
            assert!(rl.try_admit(user));
        }
        assert!(!rl.try_admit(user));
        clock.advance(Duration::from_secs(61));
        assert!(rl.try_admit(user));
    }

    #[test]
    fn per_user_independence() {
        let clock = FakeClock::new();
        let rl = TestConnectRateLimiter::new(clock);
        let a = UserId::new();
        let b = UserId::new();
        for _ in 0..MCP_TEST_CONNECT_PER_MIN {
            assert!(rl.try_admit(a));
        }
        assert!(!rl.try_admit(a));
        assert!(rl.try_admit(b));
    }
}
