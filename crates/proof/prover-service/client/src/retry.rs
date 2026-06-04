//! Retry configuration for proof requester RPC operations.

use std::time::Duration;

use backon::ExponentialBuilder;

/// Minimum delay used to avoid tight retry loops.
pub const MIN_PROOF_REQUESTER_RETRY_DELAY: Duration = Duration::from_millis(1);

/// Default maximum retry attempts for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS: u32 = 5;

/// Default initial retry delay for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_INITIAL_DELAY: Duration = Duration::from_millis(100);

/// Default maximum retry delay for requester RPC operations.
pub const DEFAULT_PROOF_REQUESTER_MAX_DELAY: Duration = Duration::from_secs(10);

/// Exponential backoff configuration for proof requester retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofRequesterRetryConfig {
    /// Maximum number of retries performed after the initial call. The total number of
    /// requester calls in a fully exhausted run is therefore `max_attempts + 1`. Zero is
    /// treated as one (a single retry); this matches the existing
    /// `base_proof_rpc::config::RetryConfig` convention in the workspace.
    pub max_attempts: u32,
    /// First delay after a retryable requester failure.
    pub initial_delay: Duration,
    /// Maximum delay between retry attempts.
    pub max_delay: Duration,
}

impl ProofRequesterRetryConfig {
    /// Creates a proof requester retry config.
    pub const fn new(max_attempts: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self { max_attempts, initial_delay, max_delay }
    }

    /// Returns the configured max attempts, clamped to at least one attempt.
    pub const fn normalized_max_attempts(&self) -> u32 {
        if self.max_attempts == 0 { 1 } else { self.max_attempts }
    }

    /// Returns the configured max delay, clamped to the minimum allowed delay.
    pub fn normalized_max_delay(&self) -> Duration {
        self.max_delay.max(MIN_PROOF_REQUESTER_RETRY_DELAY)
    }

    /// Returns the configured initial delay, clamped to the configured max delay.
    pub fn normalized_initial_delay(&self) -> Duration {
        self.initial_delay.max(MIN_PROOF_REQUESTER_RETRY_DELAY).min(self.normalized_max_delay())
    }

    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.normalized_initial_delay())
            .with_max_delay(self.normalized_max_delay())
            .with_max_times(self.normalized_max_attempts() as usize)
            .with_jitter()
    }
}

impl Default for ProofRequesterRetryConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS,
            DEFAULT_PROOF_REQUESTER_INITIAL_DELAY,
            DEFAULT_PROOF_REQUESTER_MAX_DELAY,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_max_attempts_clamps_zero_to_one() {
        let config =
            ProofRequesterRetryConfig::new(0, Duration::from_millis(10), Duration::from_millis(20));
        assert_eq!(config.normalized_max_attempts(), 1);
    }

    #[test]
    fn normalized_initial_delay_is_clamped_to_max_delay() {
        let config =
            ProofRequesterRetryConfig::new(3, Duration::from_secs(60), Duration::from_millis(50));
        assert_eq!(config.normalized_initial_delay(), Duration::from_millis(50));
    }

    #[test]
    fn normalized_max_delay_is_clamped_to_minimum() {
        let config = ProofRequesterRetryConfig::new(3, Duration::ZERO, Duration::ZERO);
        assert_eq!(config.normalized_max_delay(), MIN_PROOF_REQUESTER_RETRY_DELAY);
    }

    #[test]
    fn default_config_uses_documented_constants() {
        let config = ProofRequesterRetryConfig::default();
        assert_eq!(config.max_attempts, DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS);
        assert_eq!(config.initial_delay, DEFAULT_PROOF_REQUESTER_INITIAL_DELAY);
        assert_eq!(config.max_delay, DEFAULT_PROOF_REQUESTER_MAX_DELAY);
    }
}
