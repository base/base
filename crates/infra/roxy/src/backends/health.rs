//! EMA-based health tracking.

use std::time::{Duration, Instant};

use super::HealthTracker;
use crate::HealthStatus;

/// Configuration for health tracking.
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Latency threshold for degraded status.
    pub degraded_latency: Duration,
    /// Error rate threshold for unhealthy status.
    pub unhealthy_error_rate: f64,
    /// Minimum requests before calculating error rate.
    pub min_requests: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            degraded_latency: Duration::from_millis(500),
            unhealthy_error_rate: 0.5,
            min_requests: 10,
        }
    }
}

/// EMA-based health tracker.
#[derive(Debug)]
pub struct EmaHealthTracker {
    latency_ema: Duration,
    error_count: u64,
    request_count: u64,
    last_success: Option<Instant>,
    config: HealthConfig,
}

impl EmaHealthTracker {
    /// Create a new EMA health tracker.
    #[must_use]
    pub const fn new(config: HealthConfig) -> Self {
        Self {
            latency_ema: Duration::ZERO,
            error_count: 0,
            request_count: 0,
            last_success: None,
            config,
        }
    }
}

impl HealthTracker for EmaHealthTracker {
    fn record(&mut self, duration: Duration, success: bool) {
        self.request_count += 1;
        if success {
            self.last_success = Some(Instant::now());
        } else {
            self.error_count += 1;
        }
        // EMA: new = (old + elapsed) / 2
        self.latency_ema = (self.latency_ema + duration) / 2;
    }

    fn latency_ema(&self) -> Duration {
        self.latency_ema
    }

    fn error_rate(&self) -> f64 {
        if self.request_count < self.config.min_requests {
            return 0.0;
        }
        self.error_count as f64 / self.request_count as f64
    }

    fn status(&self) -> HealthStatus {
        let error_rate = self.error_rate();
        if error_rate >= self.config.unhealthy_error_rate {
            HealthStatus::Unhealthy { error_rate }
        } else if self.latency_ema >= self.config.degraded_latency {
            HealthStatus::Degraded { latency_ema: self.latency_ema }
        } else {
            HealthStatus::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_state() {
        let tracker = EmaHealthTracker::new(HealthConfig::default());
        assert_eq!(tracker.latency_ema(), Duration::ZERO);
        assert_eq!(tracker.error_rate(), 0.0);
        assert!(matches!(tracker.status(), HealthStatus::Healthy));
    }

    #[test]
    fn test_record_success() {
        let mut tracker = EmaHealthTracker::new(HealthConfig::default());
        tracker.record(Duration::from_millis(100), true);

        assert_eq!(tracker.latency_ema(), Duration::from_millis(50));
        assert_eq!(tracker.error_rate(), 0.0);
    }

    #[test]
    fn test_latency_ema_calculation() {
        let mut tracker = EmaHealthTracker::new(HealthConfig::default());

        // First request: 100ms -> EMA = (0 + 100) / 2 = 50ms
        tracker.record(Duration::from_millis(100), true);
        assert_eq!(tracker.latency_ema(), Duration::from_millis(50));

        // Second request: 100ms -> EMA = (50 + 100) / 2 = 75ms
        tracker.record(Duration::from_millis(100), true);
        assert_eq!(tracker.latency_ema(), Duration::from_millis(75));

        // Third request: 100ms -> EMA = (75 + 100) / 2 = 87.5ms
        // Duration division truncates, so we check the range
        tracker.record(Duration::from_millis(100), true);
        let ema = tracker.latency_ema();
        assert!(
            ema >= Duration::from_millis(87) && ema <= Duration::from_millis(88),
            "Expected ~87-88ms, got {ema:?}"
        );
    }

    #[test]
    fn test_error_rate_below_min_requests() {
        let config = HealthConfig { min_requests: 10, ..Default::default() };
        let mut tracker = EmaHealthTracker::new(config);

        // Record some failures but below min_requests threshold
        for _ in 0..5 {
            tracker.record(Duration::from_millis(10), false);
        }

        // Error rate should be 0 because we haven't hit min_requests
        assert_eq!(tracker.error_rate(), 0.0);
    }

    #[test]
    fn test_error_rate_above_min_requests() {
        let config = HealthConfig { min_requests: 5, ..Default::default() };
        let mut tracker = EmaHealthTracker::new(config);

        // Record 5 successes and 5 failures
        for _ in 0..5 {
            tracker.record(Duration::from_millis(10), true);
        }
        for _ in 0..5 {
            tracker.record(Duration::from_millis(10), false);
        }

        // Error rate should be 5/10 = 0.5
        assert_eq!(tracker.error_rate(), 0.5);
    }

    #[test]
    fn test_status_healthy() {
        let config = HealthConfig {
            degraded_latency: Duration::from_millis(500),
            unhealthy_error_rate: 0.5,
            min_requests: 1,
        };
        let mut tracker = EmaHealthTracker::new(config);

        tracker.record(Duration::from_millis(50), true);

        assert!(matches!(tracker.status(), HealthStatus::Healthy));
    }

    #[test]
    fn test_status_degraded() {
        let config = HealthConfig {
            degraded_latency: Duration::from_millis(100),
            unhealthy_error_rate: 0.5,
            min_requests: 1,
        };
        let mut tracker = EmaHealthTracker::new(config);

        // Record high latency requests
        for _ in 0..5 {
            tracker.record(Duration::from_millis(200), true);
        }

        match tracker.status() {
            HealthStatus::Degraded { latency_ema } => {
                assert!(latency_ema >= Duration::from_millis(100));
            }
            _ => panic!("Expected Degraded status"),
        }
    }

    #[test]
    fn test_status_unhealthy() {
        let config = HealthConfig {
            degraded_latency: Duration::from_millis(500),
            unhealthy_error_rate: 0.3,
            min_requests: 5,
        };
        let mut tracker = EmaHealthTracker::new(config);

        // Record mostly failures
        for _ in 0..3 {
            tracker.record(Duration::from_millis(10), true);
        }
        for _ in 0..7 {
            tracker.record(Duration::from_millis(10), false);
        }

        match tracker.status() {
            HealthStatus::Unhealthy { error_rate } => {
                assert!(error_rate >= 0.3);
            }
            _ => panic!("Expected Unhealthy status"),
        }
    }
}
