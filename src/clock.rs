//! Injectable clock per CLAUDE.md §11. Production code never calls `Instant::now` /
//! `SystemTime::now` directly — it goes through this trait so tests stay deterministic.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub trait Clock: fmt::Debug + Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn now_wall(&self) -> SystemTime;

    /// Convenience: the same instant as [`Clock::now_wall`] in UTC. Default impl
    /// is correct for every clock; subsystems use this instead of carrying their
    /// own `DateTime::<Utc>::from(clock.now_wall())` helper.
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from(self.now_wall())
    }

    /// Unix-epoch seconds. Used by every HMAC token signer (Slack
    /// install state, MCP-connect link) so all `exp` fields read the
    /// same monotonic surface. Returns `0` on the pre-epoch / overflow
    /// edges — matches the previous local helpers' behaviour.
    fn now_unix_secs(&self) -> i64 {
        let secs = self
            .now_wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        i64::try_from(secs).unwrap_or(0)
    }
}

/// Reference-counted clock handle. Subsystems hold one without taking a generic
/// parameter; cloning is `Arc::clone`.
pub type SharedClock = Arc<dyn Clock>;

/// Real wall-clock implementation. The only place in the codebase that should construct
/// this is `main` (or the integration-test harness wiring a full app).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    /// Convenience constructor used by composition roots and tests.
    #[must_use]
    pub fn shared() -> SharedClock {
        Arc::new(Self)
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn now_wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Deterministic clock for tests. Time only advances when the test calls
/// [`TestClock::advance`]; pair with `tokio::time::pause()` so tokio timers stay
/// synchronised with what the lease manager observes.
#[derive(Debug)]
pub struct TestClock {
    base: Instant,
    base_wall: SystemTime,
    offset: Mutex<Duration>,
}

impl TestClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            base_wall: SystemTime::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Move the clock forward by `delta`. Panics on overflow — tests do not push past
    /// `Duration::MAX`, so this is an assertion of an invariant.
    pub fn advance(&self, delta: Duration) {
        let mut guard = self
            .offset
            .lock()
            .expect("invariant: TestClock offset mutex never poisoned in tests");
        *guard = guard
            .checked_add(delta)
            .expect("invariant: TestClock offset must not overflow");
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        let offset = *self
            .offset
            .lock()
            .expect("invariant: TestClock offset mutex never poisoned in tests");
        self.base + offset
    }

    fn now_wall(&self) -> SystemTime {
        let offset = *self
            .offset
            .lock()
            .expect("invariant: TestClock offset mutex never poisoned in tests");
        self.base_wall + offset
    }
}
