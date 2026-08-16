//! A fixed-window rate limit on ingest.
//!
//! Scope is deliberately small: this exists so a misconfigured CI retry loop
//! or a leaked token cannot fill the database, not to withstand a distributed
//! attack. It is per-process and in-memory, so it resets on restart and two
//! servers behind a load balancer would each allow the full rate. Both are
//! acceptable for a single-container deployment and would not be for a fleet.
//!
//! Fixed windows, not a token bucket: a client can send up to twice the limit
//! across a window boundary. Tolerated because the limit is a backstop rather
//! than a quota, and the simpler structure is easier to reason about.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window length. One minute matches how the limit is expressed.
const WINDOW: Duration = Duration::from_secs(60);

/// Drop idle clients once the map grows past this.
///
/// Without a bound, a server facing many distinct source addresses would grow
/// this map without limit — the rate limiter itself becoming the memory leak
/// it exists to prevent.
const MAX_TRACKED: usize = 10_000;

struct Window {
    started: Instant,
    count: u32,
}

/// Counts requests per client key over a fixed window.
pub struct RateLimiter {
    limit: u32,
    windows: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Records one request against `key`, returning false when over the limit.
    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, Instant::now())
    }

    /// The clock is a parameter so window expiry is testable without sleeping.
    fn allow_at(&self, key: &str, now: Instant) -> bool {
        // Poisoning recovery: the data is a counter, not an invariant-bearing
        // structure, so a panic elsewhere must not turn into a permanent 500
        // on every later request.
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if windows.len() >= MAX_TRACKED {
            windows.retain(|_, window| now.duration_since(window.started) < WINDOW);
            // Still full of live windows: this many distinct clients inside one
            // window is far outside normal use, so refuse rather than grow.
            if windows.len() >= MAX_TRACKED {
                return false;
            }
        }

        let window = windows.entry(key.to_owned()).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(window.started) >= WINDOW {
            window.started = now;
            window.count = 0;
        }
        if window.count >= self.limit {
            return false;
        }
        window.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let limiter = RateLimiter::new(3);
        for attempt in 1..=3 {
            assert!(limiter.allow("client"), "attempt {attempt} must be allowed");
        }
        assert!(!limiter.allow("client"), "the fourth exceeds the limit");
    }

    #[test]
    fn clients_are_counted_separately() {
        let limiter = RateLimiter::new(1);
        assert!(limiter.allow("a"));
        assert!(!limiter.allow("a"));
        // b's budget is untouched by a exhausting its own.
        assert!(limiter.allow("b"));
    }

    #[test]
    fn the_window_resets_after_it_elapses() {
        let limiter = RateLimiter::new(2);
        let start = Instant::now();
        assert!(limiter.allow_at("client", start));
        assert!(limiter.allow_at("client", start));
        assert!(!limiter.allow_at("client", start));
        // Just inside the window: still refused.
        assert!(!limiter.allow_at("client", start + WINDOW - Duration::from_millis(1)));
        // And past it: a fresh budget.
        assert!(limiter.allow_at("client", start + WINDOW));
    }

    /// The tracking map must not grow without bound; expired windows are
    /// reclaimed once it reaches its cap.
    #[test]
    fn expired_windows_are_reclaimed() {
        let limiter = RateLimiter::new(1);
        let start = Instant::now();
        for index in 0..MAX_TRACKED {
            assert!(limiter.allow_at(&format!("client-{index}"), start));
        }
        // A new client one window later: the stale entries are swept, so this
        // is admitted rather than refused by the cap.
        assert!(limiter.allow_at("late", start + WINDOW));
        let tracked = limiter
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        assert!(tracked < MAX_TRACKED, "stale windows were not reclaimed: {tracked}");
    }

    /// A limit of zero refuses everything rather than dividing by zero or
    /// allowing one through.
    #[test]
    fn a_zero_limit_refuses_everything() {
        let limiter = RateLimiter::new(0);
        assert!(!limiter.allow("client"));
    }
}
