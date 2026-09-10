use std::time::Duration;

use backon::ExponentialBuilder;

/// Minimum retry delay used to avoid tight retry loops.
pub const MIN_RETRY_DELAY: Duration = Duration::from_millis(1);

/// Default maximum bounded retry attempts.
pub const DEFAULT_BOUNDED_MAX_ATTEMPTS: u32 = 5;

/// Default initial bounded retry delay.
pub const DEFAULT_BOUNDED_INITIAL_DELAY: Duration = Duration::from_millis(100);

/// Default maximum bounded retry delay.
pub const DEFAULT_BOUNDED_MAX_DELAY: Duration = Duration::from_secs(10);

/// Default initial unbounded retry delay.
pub const DEFAULT_UNBOUNDED_INITIAL_DELAY: Duration = Duration::from_millis(250);

/// Default maximum unbounded retry delay.
pub const DEFAULT_UNBOUNDED_MAX_DELAY: Duration = Duration::from_secs(30);

/// Retry configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// Maximum number of retries performed after the initial call.
    ///
    /// `None` retries without an attempt limit.
    pub max_attempts: Option<u32>,
    /// First delay after a retryable failure.
    pub initial_delay: Duration,
    /// Maximum delay between retry attempts.
    pub max_delay: Duration,
}

impl RetryConfig {
    /// Creates a bounded retry config.
    pub const fn new(max_attempts: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self { max_attempts: Some(max_attempts), initial_delay, max_delay }
    }

    /// Creates an unbounded retry config.
    pub const fn unbounded(initial_delay: Duration, max_delay: Duration) -> Self {
        Self { max_attempts: None, initial_delay, max_delay }
    }

    /// Returns the configured max delay, clamped to the minimum allowed delay.
    pub fn normalized_max_delay(&self) -> Duration {
        self.max_delay.max(MIN_RETRY_DELAY)
    }

    /// Returns the configured initial delay, clamped to the configured max delay.
    pub fn normalized_initial_delay(&self) -> Duration {
        self.initial_delay.max(MIN_RETRY_DELAY).min(self.normalized_max_delay())
    }

    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        let builder = ExponentialBuilder::default()
            .with_min_delay(self.normalized_initial_delay())
            .with_max_delay(self.normalized_max_delay())
            .with_jitter();

        let Some(max_attempts) = self.max_attempts else {
            return builder.without_max_times();
        };

        builder.with_max_times(max_attempts as usize)
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_BOUNDED_MAX_ATTEMPTS,
            DEFAULT_BOUNDED_INITIAL_DELAY,
            DEFAULT_BOUNDED_MAX_DELAY,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_defaults_use_configured_constants() {
        let config = RetryConfig::default();

        assert_eq!(config.max_attempts, Some(DEFAULT_BOUNDED_MAX_ATTEMPTS));
        assert_eq!(config.initial_delay, DEFAULT_BOUNDED_INITIAL_DELAY);
        assert_eq!(config.max_delay, DEFAULT_BOUNDED_MAX_DELAY);
    }

    #[test]
    fn bounded_max_attempts_preserves_zero() {
        let config = RetryConfig::new(0, Duration::from_millis(10), Duration::from_millis(20));

        assert_eq!(config.max_attempts, Some(0));
        assert!(format!("{:?}", config.to_backoff_builder()).contains("max_times: Some(0)"));
    }

    #[test]
    fn bounded_initial_delay_is_clamped_to_max_delay() {
        let config = RetryConfig::new(3, Duration::from_secs(60), Duration::from_millis(50));

        assert_eq!(config.normalized_initial_delay(), Duration::from_millis(50));
    }

    #[test]
    fn bounded_max_delay_is_clamped_to_minimum() {
        let config = RetryConfig::new(3, Duration::ZERO, Duration::ZERO);

        assert_eq!(config.normalized_max_delay(), MIN_RETRY_DELAY);
    }

    #[test]
    fn bounded_builder_sets_a_retry_limit() {
        let config = RetryConfig::new(3, Duration::from_millis(5), Duration::from_millis(12));

        let builder = config.to_backoff_builder();

        assert!(format!("{builder:?}").contains("max_times: Some(3)"));
    }

    #[test]
    fn unbounded_config_uses_configured_delays() {
        let config =
            RetryConfig::unbounded(DEFAULT_UNBOUNDED_INITIAL_DELAY, DEFAULT_UNBOUNDED_MAX_DELAY);

        assert_eq!(config.max_attempts, None);
        assert_eq!(config.initial_delay, DEFAULT_UNBOUNDED_INITIAL_DELAY);
        assert_eq!(config.max_delay, DEFAULT_UNBOUNDED_MAX_DELAY);
    }

    #[test]
    fn unbounded_builder_has_no_retry_limit() {
        let config = RetryConfig::unbounded(Duration::from_millis(5), Duration::from_millis(12));

        let builder = config.to_backoff_builder();

        assert_eq!(config.normalized_initial_delay(), Duration::from_millis(5));
        assert_eq!(config.normalized_max_delay(), Duration::from_millis(12));
        assert!(format!("{builder:?}").contains("max_times: None"));
    }
}
