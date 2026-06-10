//! Launch-period anti-farming guardrails (#121) — a single self-contained unit.
//!
//! Free public beta + anonymous Google signup means Patom pays every platform
//! LLM bill, and #154's automatic $2 signup credit is directly farmable
//! (script signups → free inference). This module holds the launch-promo
//! defenses that bound that abuse:
//!
//! - [`ClientIp`] — trusted client-IP extraction from `X-Forwarded-For`, gated
//!   on a configured reverse-proxy hop count.
//! - [`SignupRateLimiter`] — a bounded per-IP token bucket throttling
//!   OAuth-callback signup velocity.
//!
//! Everything here is **inert unless** [`crate::http::AppState::launch_guardrails`]
//! is on (the `PATOM_LAUNCH_GUARDRAILS` switch). Combined with the launch org
//! cap ([`crate::auth::limits::MAX_ORGS_PER_USER_LAUNCH`]), the whole feature
//! is one env flag to disable and one module to delete when the launch promo
//! ends — see the issue plan's "Turn-off / removal" section.
//!
//! The IP is read in-handler via [`ClientIp::from_forwarded`] (a pure function)
//! rather than an axum extractor: the callback already holds `State`, and a
//! plain function is trivially unit-testable (§3) without a request fixture.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::http::HeaderMap;

use crate::auth::limits::{SIGNUP_PER_IP_PER_WINDOW, SIGNUP_RATE_BUCKETS_MAX, SIGNUP_RATE_WINDOW};
use crate::clock::SharedClock;

/// Standard de-facto header carrying the client-IP chain through proxies.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// A client IP address Patom is willing to trust for rate-limiting.
///
/// Newtype (§1): the only way to obtain one is [`Self::from_forwarded`], which
/// applies the trusted-proxy-hop policy. A bare `IpAddr` lifted straight off an
/// untrusted header is exactly the spoofable value this type refuses to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientIp(IpAddr);

impl ClientIp {
    /// Resolve the genuine client IP from `X-Forwarded-For`, trusting exactly
    /// `trusted_hops` reverse proxies in front of the app.
    ///
    /// `X-Forwarded-For` is appended left-to-right, so the **rightmost**
    /// entries are written by the proxies closest to the app (the ones we
    /// control); entries further left are increasingly client-controlled and
    /// spoofable. With `trusted_hops` trusted proxies, the address the
    /// outermost trusted proxy observed sits `trusted_hops` from the right —
    /// index `len - trusted_hops`. A client cannot forge that position without
    /// also controlling our own proxies.
    ///
    /// Returns `None` — **fail-open** — when:
    /// - `trusted_hops == 0` (no proxy trusted; local-dev / self-host), or
    /// - the header is absent, non-ASCII, or has fewer entries than
    ///   `trusted_hops` (a chain shorter than expected is untrustworthy), or
    /// - the selected entry does not parse as an `IpAddr`.
    ///
    /// Fail-open is deliberate: a header quirk must never lock a real user out
    /// of login. The cost is that a misconfigured proxy silently disables the
    /// throttle — surfaced as the documented launch caveat, not a lockout.
    #[must_use]
    pub fn from_forwarded(headers: &HeaderMap, trusted_hops: u8) -> Option<Self> {
        if trusted_hops == 0 {
            return None;
        }
        let raw = headers.get(FORWARDED_FOR)?.to_str().ok()?;
        // Walk the chain from the right (`rsplit`): the entry `trusted_hops`
        // from the right is the address the outermost trusted proxy observed.
        // `nth(hops - 1)` lands on it in a single pass with no allocation, and
        // naturally yields `None` when there are fewer than `trusted_hops`
        // non-empty entries — a chain shorter than the proxies we sit behind
        // is untrustworthy, so we read no client-controlled entry.
        let candidate = raw
            .rsplit(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .nth(usize::from(trusted_hops) - 1)?;
        candidate.parse::<IpAddr>().ok().map(Self)
    }

    /// The underlying address. Reader only (§1) — primarily for `%`-formatting
    /// in a `debug!` during investigation; the raw IP is PII and never logged
    /// above DEBUG (§2).
    #[must_use]
    pub fn as_ip(self) -> IpAddr {
        self.0
    }
}

impl std::fmt::Display for ClientIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One bucket: timestamps of recent admits plus an LRU "last touched" stamp.
/// Mirrors the MCP test-connect limiter (`crate::mcp::rate_limit`) — the same
/// shape said the same way, kept as a sibling rather than prematurely
/// abstracted (§4: rule of three).
struct Bucket {
    /// Admit timestamps, oldest first. Capped at [`SIGNUP_PER_IP_PER_WINDOW`]
    /// by the admit loop.
    samples: VecDeque<Instant>,
    /// Last admit attempt; picks the LRU victim when the map is full.
    last_touched: Instant,
}

/// Bounded in-memory per-IP signup-velocity limiter for the OAuth callback.
///
/// Process-wide singleton, cheap to clone (`Arc<Mutex<…>>`). Each IP gets one
/// bucket recording its admits inside a rolling [`SIGNUP_RATE_WINDOW`]; a new
/// admit is allowed iff fewer than [`SIGNUP_PER_IP_PER_WINDOW`] fall in the
/// window. The map is capped at [`SIGNUP_RATE_BUCKETS_MAX`] entries with LRU
/// eviction so a flood of distinct source IPs cannot grow memory unboundedly
/// (§5). Time comes from the injected [`SharedClock`] (§11), so tests drive it
/// deterministically.
#[derive(Clone)]
pub struct SignupRateLimiter {
    inner: Arc<Mutex<HashMap<IpAddr, Bucket>>>,
    clock: SharedClock,
}

impl std::fmt::Debug for SignupRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupRateLimiter").finish_non_exhaustive()
    }
}

impl SignupRateLimiter {
    #[must_use]
    pub fn new(clock: SharedClock) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::with_capacity(64))),
            clock,
        }
    }

    /// Try to admit one signup callback for `ip`. `true` → proceed; `false` →
    /// the per-window cap is hit and the caller must 429.
    pub fn try_admit(&self, ip: ClientIp) -> bool {
        let now = self.clock.now();
        let mut guard: MutexGuard<'_, HashMap<IpAddr, Bucket>> = self
            .inner
            .lock()
            .expect("invariant: signup rate limiter mutex poisoned");

        let cutoff = now.checked_sub(SIGNUP_RATE_WINDOW);

        let bucket = guard.entry(ip.0).or_insert_with(|| Bucket {
            samples: VecDeque::with_capacity(SIGNUP_PER_IP_PER_WINDOW),
            last_touched: now,
        });
        bucket.last_touched = now;

        if let Some(cutoff) = cutoff {
            while bucket.samples.front().is_some_and(|t| *t < cutoff) {
                bucket.samples.pop_front();
            }
            // Post-prune invariant: every surviving sample is inside the window
            // (samples are append-only in non-decreasing clock order, so once
            // the front is >= cutoff the rest are too).
            assert!(bucket.samples.iter().all(|t| *t >= cutoff));
        }
        assert!(bucket.samples.len() <= SIGNUP_PER_IP_PER_WINDOW);
        if bucket.samples.len() >= SIGNUP_PER_IP_PER_WINDOW {
            return false;
        }
        bucket.samples.push_back(now);

        // We inserted at most one new bucket since the last eviction kept the
        // map at or below the cap, so we are at most one entry over it here.
        assert!(guard.len() <= SIGNUP_RATE_BUCKETS_MAX + 1);
        if guard.len() > SIGNUP_RATE_BUCKETS_MAX {
            // LRU eviction, excluding the bucket we just touched. O(n) in the
            // map cap; bounded.
            let victim = guard
                .iter()
                .filter(|(k, _)| **k != ip.0)
                .min_by_key(|(_, b)| b.last_touched)
                .map(|(k, _)| *k);
            if let Some(v) = victim {
                guard.remove(&v);
            }
        }
        true
    }
}

/// All launch-period guardrail state, bundled into one value (#121).
///
/// Held as a single `launch` field on [`crate::http::AppState`] so the entire
/// feature is one field to wire and one field to delete when the promo ends —
/// the bundling is the "removable as a single unit" property made structural,
/// and it keeps the launch-only bool out of `AppState`'s top-level field set.
/// When [`Self::enabled`] is false the promo is fully inert and behavior is
/// exactly baseline.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Master switch, from `PATOM_LAUNCH_GUARDRAILS`. `false` → baseline.
    pub enabled: bool,
    /// Trusted reverse-proxy hop count (`PATOM_TRUSTED_PROXY_HOPS`) used by
    /// [`ClientIp::from_forwarded`]. `0` → no trusted proxy → throttle inert.
    pub trusted_proxy_hops: u8,
    /// Per-IP signup-velocity limiter consulted by the OAuth callback. Built
    /// unconditionally (cheap); only consulted when [`Self::enabled`].
    pub signup_rate: SignupRateLimiter,
}

impl LaunchConfig {
    /// Build from the resolved settings and the shared clock.
    #[must_use]
    pub fn new(enabled: bool, trusted_proxy_hops: u8, clock: SharedClock) -> Self {
        Self {
            enabled,
            trusted_proxy_hops,
            signup_rate: SignupRateLimiter::new(clock),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::HeaderMap;

    use super::{ClientIp, SignupRateLimiter};
    use crate::auth::limits::SIGNUP_PER_IP_PER_WINDOW;
    use crate::clock::TestClock;

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            value.parse().expect("valid header value"),
        );
        h
    }

    fn ip(s: &str) -> ClientIp {
        ClientIp(s.parse::<IpAddr>().expect("valid ip"))
    }

    // --- ClientIp::from_forwarded ---

    #[test]
    fn hops_zero_never_trusts_any_header() {
        let h = headers_with_xff("203.0.113.7");
        assert_eq!(ClientIp::from_forwarded(&h, 0), None);
    }

    #[test]
    fn one_hop_single_entry_is_the_client() {
        let h = headers_with_xff("203.0.113.7");
        assert_eq!(
            ClientIp::from_forwarded(&h, 1).map(ClientIp::as_ip),
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
        );
    }

    #[test]
    fn one_hop_takes_rightmost_entry() {
        // The rightmost entry is the one our single trusted proxy appended;
        // anything further left is client-controlled and must be ignored.
        let h = headers_with_xff("1.1.1.1, 203.0.113.7");
        assert_eq!(
            ClientIp::from_forwarded(&h, 1).map(ClientIp::as_ip),
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
        );
    }

    #[test]
    fn two_hops_takes_second_from_right() {
        // CDN + ingress both append: rightmost is the CDN egress, the client
        // is the entry the CDN observed (second from the right).
        let h = headers_with_xff("9.9.9.9, 203.0.113.7, 10.0.0.1");
        assert_eq!(
            ClientIp::from_forwarded(&h, 2).map(ClientIp::as_ip),
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
        );
    }

    #[test]
    fn chain_shorter_than_hops_is_untrusted() {
        let h = headers_with_xff("203.0.113.7");
        assert_eq!(ClientIp::from_forwarded(&h, 2), None);
    }

    #[test]
    fn absent_header_is_none() {
        let h = HeaderMap::new();
        assert_eq!(ClientIp::from_forwarded(&h, 1), None);
    }

    #[test]
    fn malformed_entry_is_none() {
        let h = headers_with_xff("not-an-ip");
        assert_eq!(ClientIp::from_forwarded(&h, 1), None);
    }

    // --- SignupRateLimiter (clock owned by the test, §11) ---

    #[test]
    fn admits_up_to_cap_then_rejects() {
        let rl = SignupRateLimiter::new(Arc::new(TestClock::new()));
        let client = ip("203.0.113.7");
        for _ in 0..SIGNUP_PER_IP_PER_WINDOW {
            assert!(rl.try_admit(client));
        }
        assert!(!rl.try_admit(client));
    }

    #[test]
    fn window_slides() {
        let clock = Arc::new(TestClock::new());
        let rl = SignupRateLimiter::new(clock.clone());
        let client = ip("203.0.113.7");
        for _ in 0..SIGNUP_PER_IP_PER_WINDOW {
            assert!(rl.try_admit(client));
        }
        assert!(!rl.try_admit(client));
        clock.advance(Duration::from_secs(61));
        assert!(rl.try_admit(client));
    }

    #[test]
    fn per_ip_independence() {
        let rl = SignupRateLimiter::new(Arc::new(TestClock::new()));
        let a = ip("203.0.113.7");
        let b = ip("203.0.113.8");
        for _ in 0..SIGNUP_PER_IP_PER_WINDOW {
            assert!(rl.try_admit(a));
        }
        assert!(!rl.try_admit(a));
        assert!(rl.try_admit(b));
    }
}
