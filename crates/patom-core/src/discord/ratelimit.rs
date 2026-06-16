//! Outbound rate limiting — the two genuinely-new hazards Discord adds.
//!
//! 1. **Global per-token limit (~50 req/s per bot).** A token bucket per
//!    `application_id` throttles proactively so a burst of replies across many
//!    threads does not trip Discord's global limit. The per-route 429 (with
//!    `Retry-After`) is handled reactively in the poster's retry loop.
//! 2. **Cloudflare invalid-request ban.** More than
//!    [`DISCORD_INVALID_REQUEST_BUDGET`] `401/403/429` responses in a 10-minute
//!    window bans the **egress IP** — which aggregates across every tenant bot on
//!    the shared egress, so one noisy tenant can take the rest down. We count
//!    invalids in a single per-egress window and warn as it approaches the cap.
//!
//! Time comes from `tokio::time::Instant`, so tests drive it with paused time.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::time::{Instant, sleep};
use tracing::warn;

use super::limits::{
    DISCORD_GLOBAL_RATE_PER_SEC, DISCORD_INVALID_REQUEST_BUDGET, DISCORD_INVALID_REQUEST_WINDOW,
};
use super::types::ApplicationId;

/// A leaky/token bucket: `tokens` refill at `rate`/s up to `cap`.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(now: Instant) -> Self {
        // Start full so a fresh bot may burst up to the cap.
        Self {
            tokens: f64::from(DISCORD_GLOBAL_RATE_PER_SEC),
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        let cap = f64::from(DISCORD_GLOBAL_RATE_PER_SEC);
        self.tokens = elapsed
            .mul_add(f64::from(DISCORD_GLOBAL_RATE_PER_SEC), self.tokens)
            .min(cap);
        self.last = now;
    }

    /// Try to consume one token; on success return `None`, else the wait until
    /// one token is available.
    fn try_take(&mut self, now: Instant) -> Option<Duration> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let deficit = 1.0 - self.tokens;
            Some(Duration::from_secs_f64(
                deficit / f64::from(DISCORD_GLOBAL_RATE_PER_SEC),
            ))
        }
    }
}

/// A per-egress window of invalid (`401/403/429`) responses.
#[derive(Debug)]
struct InvalidBudget {
    window_start: Instant,
    count: u32,
}

/// The outbound rate limiter (cheap-clone-free; held behind an `Arc`).
#[derive(Debug)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<ApplicationId, TokenBucket>>,
    invalid: Mutex<InvalidBudget>,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            invalid: Mutex::new(InvalidBudget {
                window_start: Instant::now(),
                count: 0,
            }),
        }
    }

    /// Acquire one global token for `app`, waiting (proactively) if the bot is at
    /// its ~50/s ceiling. Bounded: each wait is at most one token-time.
    pub async fn acquire(&self, app: &ApplicationId) {
        // Bounded retry: the bucket refills continuously, so a token is available
        // within at most `cap / rate` seconds; the loop turns at most a few times.
        for _ in 0..=DISCORD_GLOBAL_RATE_PER_SEC {
            let wait = {
                let now = Instant::now();
                let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
                let bucket = buckets
                    .entry(app.clone())
                    .or_insert_with(|| TokenBucket::new(now));
                bucket.try_take(now)
            };
            match wait {
                None => return,
                Some(d) => sleep(d).await,
            }
        }
    }

    /// Record an invalid (`401/403/429`) response and warn as the per-egress
    /// budget approaches the Cloudflare ban threshold. Returns the running count.
    pub fn record_invalid(&self) -> u32 {
        let now = Instant::now();
        let mut budget = self.invalid.lock().unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(budget.window_start) > DISCORD_INVALID_REQUEST_WINDOW {
            budget.window_start = now;
            budget.count = 0;
        }
        budget.count = budget.count.saturating_add(1);
        let count = budget.count;
        // Warn from 80% of the budget so an operator can react before the ban.
        if count >= DISCORD_INVALID_REQUEST_BUDGET / 5 * 4 {
            warn!(
                event = "discord.ratelimit.invalid_budget_high",
                count,
                budget = DISCORD_INVALID_REQUEST_BUDGET,
            );
        }
        count
    }

    /// Whether the per-egress invalid-request budget is exhausted (a Cloudflare
    /// ban is imminent / active) — the poster stops sending to avoid worsening it.
    #[must_use]
    pub fn invalid_budget_exhausted(&self) -> bool {
        let now = Instant::now();
        let budget = self.invalid.lock().unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(budget.window_start) > DISCORD_INVALID_REQUEST_WINDOW {
            return false;
        }
        budget.count >= DISCORD_INVALID_REQUEST_BUDGET
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> ApplicationId {
        ApplicationId::try_from("123456789012345678").expect("app id")
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_allows_a_burst_then_throttles() {
        let rl = RateLimiter::new();
        let a = app();
        // The first `cap` acquires return immediately (the full burst).
        for _ in 0..DISCORD_GLOBAL_RATE_PER_SEC {
            rl.acquire(&a).await;
        }
        // The next acquire must wait for a refill; prove it by racing a 0-advance
        // timeout — without advancing time it cannot complete.
        let immediate = tokio::time::timeout(Duration::from_millis(0), rl.acquire(&a)).await;
        assert!(
            immediate.is_err(),
            "the {DISCORD_GLOBAL_RATE_PER_SEC}+1-th acquire must wait"
        );
        // After ~1s of refill, a token is available again.
        tokio::time::advance(Duration::from_secs(1)).await;
        rl.acquire(&a).await;
    }

    #[tokio::test(start_paused = true)]
    async fn buckets_are_per_app() {
        let rl = RateLimiter::new();
        let a = ApplicationId::try_from("111111111111111111").expect("a");
        let b = ApplicationId::try_from("222222222222222222").expect("b");
        // Drain app a's bucket entirely…
        for _ in 0..DISCORD_GLOBAL_RATE_PER_SEC {
            rl.acquire(&a).await;
        }
        // …app b still has its own full bucket (no cross-talk).
        let ok = tokio::time::timeout(Duration::from_millis(0), rl.acquire(&b)).await;
        assert!(ok.is_ok(), "a different bot has an independent bucket");
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_budget_counts_and_windows() {
        let rl = RateLimiter::new();
        assert_eq!(rl.record_invalid(), 1);
        assert_eq!(rl.record_invalid(), 2);
        assert!(!rl.invalid_budget_exhausted());
        // After the window elapses, the counter resets.
        tokio::time::advance(DISCORD_INVALID_REQUEST_WINDOW + Duration::from_secs(1)).await;
        assert_eq!(rl.record_invalid(), 1, "a new window restarts the count");
    }
}
