//! Byte-rate limiting with a token bucket shared by any number of download tasks.
//!
//! Workers call [`RateLimiter::acquire`] after each chunk they receive. The call
//! sleeps when the bucket is in debt, so the long-run rate stays at the limit.
//! Concurrent callers push the bucket further into debt and wait longer in turn.
//!
//! Limiters chain: a worker that has a global and a per-download limit calls
//! [`acquire_all`] with both.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::time::Instant;

/// Shared byte-rate limit. Cheap to call when unlimited.
#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Bytes per second. `None` means unlimited.
    rate: Option<u64>,
    /// Bytes available. May go negative (debt).
    tokens: f64,
    /// Time of the last refill.
    last: Instant,
}

/// Bucket size for a rate: 100 ms worth of bytes, and at least one byte.
fn burst_for(rate: u64) -> f64 {
    (rate as f64 * 0.1).max(1.0)
}

/// Turns a requested limit into the stored one. A limit of zero becomes one.
fn clamp_rate(bytes_per_sec: Option<u64>) -> Option<u64> {
    bytes_per_sec.map(|r| r.max(1))
}

impl State {
    /// Adds the tokens earned since `last` at the current rate, capped at the burst.
    fn refill(&mut self, now: Instant) {
        if let Some(rate) = self.rate {
            let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * rate as f64).min(burst_for(rate));
        }
        self.last = now;
    }
}

impl RateLimiter {
    /// Creates a limiter. `None` means unlimited. `Some(0)` is treated as `Some(1)`.
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            state: Mutex::new(State {
                rate: clamp_rate(bytes_per_sec),
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    /// The current limit in bytes per second, or `None` if unlimited.
    pub fn rate(&self) -> Option<u64> {
        self.lock().rate
    }

    /// Changes the limit. Takes effect for subsequent [`acquire`](Self::acquire) calls.
    pub fn set_rate(&self, bytes_per_sec: Option<u64>) {
        let new = clamp_rate(bytes_per_sec);
        let mut st = self.lock();
        let now = Instant::now();
        // Earn tokens at the old rate before switching.
        st.refill(now);
        match new {
            // Unlimited, or coming from unlimited: start with an empty bucket.
            None => st.tokens = 0.0,
            Some(_) if st.rate.is_none() => st.tokens = 0.0,
            // Between two limits: keep the balance (including any debt), capped at the new burst.
            Some(rate) => st.tokens = st.tokens.min(burst_for(rate)),
        }
        st.rate = new;
        st.last = now;
    }

    /// Accounts for `n` bytes, sleeping if the bucket is in debt.
    pub async fn acquire(&self, n: u64) {
        let wait = {
            let mut st = self.lock();
            let Some(rate) = st.rate else {
                return;
            };
            st.refill(Instant::now());
            st.tokens -= n as f64;
            if st.tokens < 0.0 { -st.tokens / rate as f64 } else { 0.0 }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    /// Locks the state, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Acquires `n` bytes from each limiter in turn, e.g. global first, then per-download.
pub async fn acquire_all(limiters: &[Arc<RateLimiter>], n: u64) {
    for limiter in limiters {
        limiter.acquire(n).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acquires `total` bytes in chunks of `chunk` bytes.
    async fn transfer(limiter: &RateLimiter, mut total: u64, chunk: u64) {
        while total > 0 {
            let n = chunk.min(total);
            limiter.acquire(n).await;
            total -= n;
        }
    }

    /// Asserts that `actual` is within `tolerance` (a fraction) of `expected`.
    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        let err = (actual - expected).abs() / expected;
        assert!(
            err <= tolerance,
            "elapsed {actual:.6}s, expected {expected:.6}s (off by {:.3}%)",
            err * 100.0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let limiter = RateLimiter::new(None);
        let start = Instant::now();
        for _ in 0..10 {
            limiter.acquire(1 << 30).await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn single_task_rate() {
        let limiter = RateLimiter::new(Some(100_000));
        let start = Instant::now();
        transfer(&limiter, 1_000_000, 16 * 1024).await;
        assert_close(start.elapsed().as_secs_f64(), 10.0, 0.01);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_tasks_share_the_rate() {
        let limiter = Arc::new(RateLimiter::new(Some(100_000)));
        let start = Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let limiter = Arc::clone(&limiter);
                tokio::spawn(async move { transfer(&limiter, 100_000, 10_000).await })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
        assert_close(start.elapsed().as_secs_f64(), 4.0, 0.02);
    }

    #[tokio::test(start_paused = true)]
    async fn set_rate_mid_transfer() {
        let limiter = RateLimiter::new(Some(100_000));
        let start = Instant::now();
        transfer(&limiter, 200_000, 16 * 1024).await;
        limiter.set_rate(Some(200_000));
        transfer(&limiter, 400_000, 16 * 1024).await;
        assert_close(start.elapsed().as_secs_f64(), 4.0, 0.03);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_to_unlimited() {
        let limiter = RateLimiter::new(Some(100_000));
        transfer(&limiter, 50_000, 10_000).await;
        limiter.set_rate(None);
        assert_eq!(limiter.rate(), None);
        let start = Instant::now();
        limiter.acquire(10 << 30).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_is_clamped() {
        assert_eq!(RateLimiter::new(Some(0)).rate(), Some(1));
        let limiter = RateLimiter::new(None);
        limiter.set_rate(Some(0));
        assert_eq!(limiter.rate(), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_all_is_bound_by_the_slowest() {
        let global = Arc::new(RateLimiter::new(Some(1_000_000)));
        let per_download = Arc::new(RateLimiter::new(Some(50_000)));
        let limiters = [global, per_download];
        let start = Instant::now();
        for _ in 0..20 {
            acquire_all(&limiters, 10_000).await;
        }
        assert_close(start.elapsed().as_secs_f64(), 4.0, 0.03);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_only_grants_a_small_burst() {
        let limiter = RateLimiter::new(Some(100_000));
        tokio::time::sleep(Duration::from_secs(10)).await;
        let start = Instant::now();
        limiter.acquire(100_000).await;
        // Burst is 10_000 bytes, so only 90_000 bytes need waiting for.
        assert_close(start.elapsed().as_secs_f64(), 0.9, 0.01);
    }
}
