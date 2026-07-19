//! Per-IP rate limiter for admin auth failures.
//!
//! Sliding-window count: each IP gets up to `limit` failed-auth attempts
//! within `window`. Once the limit is exceeded the IP is throttled for the
//! remainder of the window. Successful auth does NOT decrement the bucket —
//! we only care about brute-force protection. Best-effort: a bursty attacker
//! across many IPs is not blocked. Mirror of Python admin's behaviour.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;

#[derive(Debug)]
struct Bucket {
    /// Timestamps of the failures still inside the active window.
    fails: Vec<Instant>,
}

impl Bucket {
    fn prune(&mut self, now: Instant, window: Duration) {
        self.fails.retain(|t| now.duration_since(*t) <= window);
    }
}

/// Thread-safe rate limiter keyed by IP. Cheap to clone — internals are an
/// `Arc<DashMap>` by way of the surrounding `Arc<RateLimiter>`.
pub struct RateLimiter {
    buckets: DashMap<IpAddr, Mutex<Bucket>>,
    limit: usize,
    window: Duration,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            buckets: DashMap::new(),
            limit,
            window,
        }
    }

    /// Returns true when `ip` has crossed the failure budget in the current
    /// window. Call BEFORE attempting auth.
    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        if let Some(entry) = self.buckets.get(&ip) {
            let mut bucket = entry.lock();
            bucket.prune(now, self.window);
            return bucket.fails.len() >= self.limit;
        }
        false
    }

    /// Record one failed auth attempt for `ip`.
    pub fn record_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let entry = self
            .buckets
            .entry(ip)
            .or_insert_with(|| Mutex::new(Bucket { fails: Vec::new() }));
        let mut bucket = entry.lock();
        bucket.prune(now, self.window);
        bucket.fails.push(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    }

    #[test]
    fn blocks_after_limit() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        assert!(!rl.is_blocked(ip()));
        rl.record_failure(ip());
        rl.record_failure(ip());
        rl.record_failure(ip());
        assert!(rl.is_blocked(ip()));
    }

    #[test]
    fn window_expires_clears_block() {
        let rl = RateLimiter::new(2, Duration::from_millis(50));
        rl.record_failure(ip());
        rl.record_failure(ip());
        assert!(rl.is_blocked(ip()));
        std::thread::sleep(Duration::from_millis(80));
        assert!(!rl.is_blocked(ip()));
    }
}
