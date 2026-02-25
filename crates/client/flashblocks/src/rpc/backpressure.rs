//! Adaptive concurrency control for builder RPC calls.
//!
//! Implements AIMD (Additive Increase Multiplicative Decrease) backpressure
//! to dynamically adjust concurrency based on success/failure rates.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio::sync::{Semaphore, SemaphorePermit};

/// Minimum concurrency limit (never stop completely).
const MIN_CONCURRENCY_LIMIT: usize = 1;

/// Per-builder RPC statistics for observability.
///
/// Tracks success and error counts for a single builder endpoint,
/// enabling per-builder health monitoring via metrics.
#[derive(Debug)]
pub struct BuilderRpcStats {
    builder_url: String,
    success_count: AtomicU64,
    error_count: AtomicU64,
}

impl BuilderRpcStats {
    /// Creates new stats tracker for a builder endpoint.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            builder_url: url.into(),
            success_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
        }
    }

    /// Records a successful RPC call.
    pub fn on_success(&self) {
        self.success_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a failed RPC call.
    pub fn on_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the total success count.
    pub fn success_count(&self) -> u64 {
        self.success_count.load(Ordering::Relaxed)
    }

    /// Returns the total error count.
    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    /// Returns the builder URL.
    pub fn url(&self) -> &str {
        &self.builder_url
    }
}

/// AIMD-style adaptive concurrency limiter.
///
/// Uses a static semaphore as a hard ceiling and an atomic soft limit
/// that adjusts dynamically based on success/failure feedback:
/// - On success: `soft_limit = min(soft_limit + 1, max_limit)` (additive increase)
/// - On error: `soft_limit = max(soft_limit / 2, 1)` (multiplicative decrease)
#[derive(Debug)]
pub struct AdaptiveConcurrencyLimiter {
    /// Hard ceiling - static semaphore that never changes size.
    hard_limit: Semaphore,
    /// Current soft limit (AIMD-adjusted). Requests check this before acquiring.
    soft_limit: AtomicUsize,
    /// Current number of in-flight requests.
    in_flight: AtomicUsize,
    /// Maximum allowed limit (for additive increase cap).
    max_limit: usize,
}

impl AdaptiveConcurrencyLimiter {
    /// Creates a new adaptive limiter with the given maximum concurrency.
    ///
    /// The soft limit starts at `max_limit` and adjusts based on feedback.
    pub fn new(max_limit: usize) -> Self {
        let max_limit = max_limit.max(MIN_CONCURRENCY_LIMIT);
        Self {
            hard_limit: Semaphore::new(max_limit),
            soft_limit: AtomicUsize::new(max_limit),
            in_flight: AtomicUsize::new(0),
            max_limit,
        }
    }

    /// Attempts to acquire a permit for an in-flight request.
    ///
    /// Returns `None` if the soft limit has been reached or the hard limit
    /// semaphore has no available permits.
    pub fn try_acquire(&self) -> Option<AdaptivePermit<'_>> {
        // Check soft limit first (fast path)
        let current_in_flight = self.in_flight.load(Ordering::Relaxed);
        let current_soft_limit = self.soft_limit.load(Ordering::Relaxed);

        if current_in_flight >= current_soft_limit {
            return None;
        }

        // Try to acquire from hard limit semaphore
        let semaphore_permit = self.hard_limit.try_acquire().ok()?;

        // Increment in-flight counter
        self.in_flight.fetch_add(1, Ordering::Relaxed);

        Some(AdaptivePermit { limiter: self, _semaphore_permit: semaphore_permit })
    }

    /// Records a successful request - additive increase.
    ///
    /// Increases the soft limit by 1, up to the maximum.
    pub fn on_success(&self) {
        let current = self.soft_limit.load(Ordering::Relaxed);
        let new_limit = (current + 1).min(self.max_limit);
        self.soft_limit.store(new_limit, Ordering::Relaxed);
    }

    /// Records a failed request - multiplicative decrease.
    ///
    /// Halves the soft limit, with a minimum of 1.
    pub fn on_error(&self) {
        let current = self.soft_limit.load(Ordering::Relaxed);
        let new_limit = (current / 2).max(MIN_CONCURRENCY_LIMIT);
        self.soft_limit.store(new_limit, Ordering::Relaxed);
    }

    /// Returns the current soft limit.
    pub fn current_limit(&self) -> usize {
        self.soft_limit.load(Ordering::Relaxed)
    }

    /// Returns the current number of in-flight requests.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Returns the maximum limit.
    pub const fn max_limit(&self) -> usize {
        self.max_limit
    }
}

/// RAII guard that decrements the in-flight counter on drop.
#[derive(Debug)]
pub struct AdaptivePermit<'a> {
    limiter: &'a AdaptiveConcurrencyLimiter,
    _semaphore_permit: SemaphorePermit<'a>,
}

impl Drop for AdaptivePermit<'_> {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_rpc_stats_counting() {
        let stats = BuilderRpcStats::new("http://builder:8545");
        assert_eq!(stats.success_count(), 0);
        assert_eq!(stats.error_count(), 0);

        stats.on_success();
        stats.on_success();
        stats.on_error();

        assert_eq!(stats.success_count(), 2);
        assert_eq!(stats.error_count(), 1);
        assert_eq!(stats.url(), "http://builder:8545");
    }

    #[test]
    fn test_adaptive_limiter_respects_soft_limit() {
        let limiter = AdaptiveConcurrencyLimiter::new(10);
        assert_eq!(limiter.current_limit(), 10);
        assert_eq!(limiter.in_flight(), 0);

        // Acquire up to soft limit
        let permits: Vec<_> = (0..10).filter_map(|_| limiter.try_acquire()).collect();
        assert_eq!(permits.len(), 10);
        assert_eq!(limiter.in_flight(), 10);

        // Should fail - at soft limit
        assert!(limiter.try_acquire().is_none());
    }

    #[test]
    fn test_aimd_additive_increase() {
        let limiter = AdaptiveConcurrencyLimiter::new(10);

        // Simulate errors reducing limit
        limiter.on_error(); // 10 -> 5
        limiter.on_error(); // 5 -> 2
        assert_eq!(limiter.current_limit(), 2);

        // Additive increase
        limiter.on_success();
        assert_eq!(limiter.current_limit(), 3);

        limiter.on_success();
        assert_eq!(limiter.current_limit(), 4);
    }

    #[test]
    fn test_aimd_additive_increase_capped_at_max() {
        let limiter = AdaptiveConcurrencyLimiter::new(5);
        assert_eq!(limiter.current_limit(), 5);

        // Should not exceed max
        limiter.on_success();
        assert_eq!(limiter.current_limit(), 5);
    }

    #[test]
    fn test_aimd_multiplicative_decrease() {
        let limiter = AdaptiveConcurrencyLimiter::new(10);

        limiter.on_error();
        assert_eq!(limiter.current_limit(), 5); // 10 / 2

        limiter.on_error();
        assert_eq!(limiter.current_limit(), 2); // 5 / 2 = 2 (floor)

        limiter.on_error();
        assert_eq!(limiter.current_limit(), 1); // 2 / 2 = 1

        limiter.on_error();
        assert_eq!(limiter.current_limit(), 1); // stays at min
    }

    #[test]
    fn test_permit_releases_on_drop() {
        let limiter = AdaptiveConcurrencyLimiter::new(2);

        {
            let _p1 = limiter.try_acquire().unwrap();
            let _p2 = limiter.try_acquire().unwrap();
            assert_eq!(limiter.in_flight(), 2);
            assert!(limiter.try_acquire().is_none());
        }

        // Permits dropped
        assert_eq!(limiter.in_flight(), 0);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn test_min_limit_enforced() {
        // Even with 0, should enforce minimum of 1
        let limiter = AdaptiveConcurrencyLimiter::new(0);
        assert_eq!(limiter.current_limit(), 1);
        assert_eq!(limiter.max_limit(), 1);
    }

    #[test]
    fn test_concurrent_acquire_and_release() {
        let limiter = AdaptiveConcurrencyLimiter::new(3);

        // Acquire all permits
        let p1 = limiter.try_acquire().unwrap();
        let p2 = limiter.try_acquire().unwrap();
        let p3 = limiter.try_acquire().unwrap();
        assert_eq!(limiter.in_flight(), 3);
        assert!(limiter.try_acquire().is_none());

        // Drop one, should be able to acquire again
        drop(p1);
        assert_eq!(limiter.in_flight(), 2);
        let p4 = limiter.try_acquire().unwrap();
        assert_eq!(limiter.in_flight(), 3);

        // Cleanup
        drop(p2);
        drop(p3);
        drop(p4);
        assert_eq!(limiter.in_flight(), 0);
    }

    #[test]
    fn test_soft_limit_reduces_available_permits() {
        let limiter = AdaptiveConcurrencyLimiter::new(10);

        // Reduce soft limit via errors
        limiter.on_error(); // 10 -> 5
        assert_eq!(limiter.current_limit(), 5);

        // Can only acquire up to soft limit
        let permits: Vec<_> = (0..10).filter_map(|_| limiter.try_acquire()).collect();
        assert_eq!(permits.len(), 5);
        assert_eq!(limiter.in_flight(), 5);
    }

    #[test]
    fn test_recovery_after_errors() {
        let limiter = AdaptiveConcurrencyLimiter::new(10);

        // Hammer with errors to reduce limit
        for _ in 0..5 {
            limiter.on_error();
        }
        assert_eq!(limiter.current_limit(), 1); // Should be at minimum

        // Recover with successes
        for _ in 0..9 {
            limiter.on_success();
        }
        assert_eq!(limiter.current_limit(), 10); // Back to max

        // One more success shouldn't exceed max
        limiter.on_success();
        assert_eq!(limiter.current_limit(), 10);
    }

    #[test]
    fn test_stats_are_independent_per_builder() {
        let stats1 = BuilderRpcStats::new("http://builder1:8545");
        let stats2 = BuilderRpcStats::new("http://builder2:8545");

        stats1.on_success();
        stats1.on_success();
        stats2.on_error();

        assert_eq!(stats1.success_count(), 2);
        assert_eq!(stats1.error_count(), 0);
        assert_eq!(stats2.success_count(), 0);
        assert_eq!(stats2.error_count(), 1);
    }

    #[test]
    fn test_limiter_stress_many_errors() {
        let limiter = AdaptiveConcurrencyLimiter::new(1000);

        // Even 100 errors shouldn't go below 1
        for _ in 0..100 {
            limiter.on_error();
        }
        assert_eq!(limiter.current_limit(), 1);

        // Still able to acquire 1
        let permit = limiter.try_acquire();
        assert!(permit.is_some());
        assert_eq!(limiter.in_flight(), 1);
    }
}
