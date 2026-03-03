use std::time::Duration;

use backon::ExponentialBuilder;

/// Default LRU cache size for RPC responses.
pub const DEFAULT_CACHE_SIZE: usize = 1000;

/// Default maximum number of retry attempts for RPC operations.
pub const DEFAULT_RPC_MAX_RETRIES: u32 = 5;

/// Default initial delay for exponential backoff.
pub const DEFAULT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);

/// Default maximum delay between retry attempts.
pub const DEFAULT_RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

/// Validated RPC retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_attempts: u32,
    /// Initial delay for exponential backoff.
    pub initial_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RPC_MAX_RETRIES,
            initial_delay: DEFAULT_RETRY_INITIAL_DELAY,
            max_delay: DEFAULT_RETRY_MAX_DELAY,
        }
    }
}

impl RetryConfig {
    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.initial_delay)
            .with_max_delay(self.max_delay)
            .with_max_times(self.max_attempts as usize)
            .with_jitter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_delay, Duration::from_millis(100));
        assert_eq!(config.max_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_retry_config_to_backoff_builder() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(5),
        };
        // Just verify it can be built without panicking
        let _ = config.to_backoff_builder();
    }
}
