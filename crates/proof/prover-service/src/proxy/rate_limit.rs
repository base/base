use std::{num::NonZeroU32, sync::Arc, time::Duration};

use governor::{Quota, RateLimiter as GovernorRateLimiter};
use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::timeout,
};
use tracing::{debug, warn};

/// Combined rate limiter that enforces both RPS limits and concurrency limits
#[derive(Clone)]
pub(super) struct RateLimiter {
    /// Token bucket rate limiter (requests per second)
    governor: Arc<
        GovernorRateLimiter<
            governor::state::direct::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    /// Semaphore for limiting concurrent requests
    semaphore: Arc<Semaphore>,
    /// Timeout for waiting in queue
    queue_timeout: Duration,
    /// Name for logging
    name: String,
}

impl RateLimiter {
    pub(super) fn new(
        requests_per_second: u32,
        max_concurrent: usize,
        queue_timeout: Duration,
        name: String,
    ) -> anyhow::Result<Self> {
        // Create token bucket rate limiter
        let rps = NonZeroU32::new(requests_per_second)
            .ok_or_else(|| anyhow::anyhow!("requests_per_second must be > 0"))?;
        let quota = Quota::per_second(rps);
        let governor = Arc::new(GovernorRateLimiter::direct(quota));

        // Create semaphore for concurrency limiting
        let semaphore = Arc::new(Semaphore::new(max_concurrent));

        debug!(
            name = %name,
            requests_per_second = %requests_per_second,
            max_concurrent = max_concurrent,
            queue_timeout_secs = queue_timeout.as_secs(),
            "Created rate limiter"
        );

        Ok(Self { governor, semaphore, queue_timeout, name })
    }

    /// Acquire permission to make a request.
    /// Returns a permit that must be held for the duration of the request.
    /// Returns None if the request should be rejected (timeout exceeded).
    pub(super) async fn acquire(&self) -> Option<SemaphorePermit<'_>> {
        // First, acquire semaphore permit (concurrency limit) with timeout
        let permit_result = timeout(self.queue_timeout, self.semaphore.acquire()).await;

        let permit = match permit_result {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                // Semaphore closed, should not happen
                warn!(name = %self.name, "Semaphore closed");
                return None;
            }
            Err(_) => {
                // Timeout waiting for semaphore
                warn!(
                    name = %self.name,
                    queue_timeout_secs = self.queue_timeout.as_secs(),
                    "Request timed out waiting for semaphore"
                );
                return None;
            }
        };

        // Then, check token bucket rate limiter
        // This waits until a token is available
        self.governor.until_ready().await;

        debug!(name = %self.name, "Request allowed");
        Some(permit)
    }

    /// Get current available permits (for monitoring)
    #[allow(dead_code)]
    pub(super) fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn test_concurrency_limit() {
        let limiter = RateLimiter::new(
            100, // High RPS to not interfere
            2,   // Only 2 concurrent
            Duration::from_secs(1),
            "test".to_string(),
        )
        .unwrap();

        // Acquire 2 permits
        let _permit1 = limiter.acquire().await.expect("Should get permit 1");
        let _permit2 = limiter.acquire().await.expect("Should get permit 2");

        // Third request should timeout
        let start = Instant::now();
        let permit3 = limiter.acquire().await;
        let elapsed = start.elapsed();

        assert!(permit3.is_none(), "Third request should timeout");
        assert!(elapsed >= Duration::from_secs(1), "Should wait for timeout duration");
    }

    #[tokio::test]
    async fn test_rate_limit() {
        let limiter = RateLimiter::new(
            2,  // 2 requests per second
            10, // High concurrency to not interfere
            Duration::from_secs(5),
            "test".to_string(),
        )
        .unwrap();

        let start = Instant::now();

        // Make 3 requests - third should be delayed
        for i in 0..3 {
            let permit = limiter.acquire().await.expect("Should get permit");
            drop(permit); // Release immediately
            if i == 2 {
                let elapsed = start.elapsed();
                // Third request should be delayed by ~500ms (to stay under 2 RPS)
                assert!(
                    elapsed >= Duration::from_millis(400),
                    "Rate limiting should delay request"
                );
            }
        }
    }
}
