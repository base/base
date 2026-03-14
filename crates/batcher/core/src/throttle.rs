//! Throttle controller for DA backlog management.

/// Configuration for the throttle controller.
#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    /// Backlog threshold in bytes at which throttling activates.
    pub threshold_bytes: u64,
    /// Maximum throttle intensity (0.0 to 1.0).
    pub max_intensity: f64,
}

/// Parameters to apply when throttling is active.
#[derive(Debug, Clone, Copy)]
pub struct ThrottleParams {
    /// Fraction of normal submission rate to apply (0.0 to 1.0).
    pub intensity: f64,
}

/// Strategy for calculating throttle intensity from DA backlog.
#[derive(Debug, Clone)]
pub enum ThrottleStrategy {
    /// No throttling.
    Off,
    /// Step function: either 0 or `max_intensity` when above threshold.
    Step,
    /// Linear interpolation between 0 and `max_intensity` based on backlog.
    Linear,
}

/// Controls submission rate based on DA backlog.
///
/// The controller evaluates the current DA backlog against a configured
/// threshold and strategy to produce throttle parameters that the driver
/// can use to slow block production on the sequencer.
#[derive(Debug)]
pub struct ThrottleController {
    /// Throttle configuration.
    config: ThrottleConfig,
    /// Strategy for computing throttle intensity.
    strategy: ThrottleStrategy,
}

impl ThrottleController {
    /// Create a new [`ThrottleController`].
    pub const fn new(config: ThrottleConfig, strategy: ThrottleStrategy) -> Self {
        Self { config, strategy }
    }

    /// Update with current DA backlog bytes.
    ///
    /// Returns [`ThrottleParams`] if throttling should be applied, or `None`
    /// if the backlog is below the threshold or the strategy is
    /// [`ThrottleStrategy::Off`].
    pub fn update(&self, da_backlog_bytes: u64) -> Option<ThrottleParams> {
        match &self.strategy {
            ThrottleStrategy::Off => None,
            ThrottleStrategy::Step => {
                if da_backlog_bytes >= self.config.threshold_bytes {
                    Some(ThrottleParams { intensity: self.config.max_intensity })
                } else {
                    None
                }
            }
            ThrottleStrategy::Linear => {
                if da_backlog_bytes < self.config.threshold_bytes {
                    return None;
                }
                // Linear interpolation: intensity grows linearly from 0 at threshold
                // to max_intensity at 2x threshold (capped at max_intensity).
                let excess = da_backlog_bytes - self.config.threshold_bytes;
                let range = self.config.threshold_bytes.max(1);
                let ratio = (excess as f64 / range as f64).min(1.0);
                let intensity = ratio * self.config.max_intensity;
                Some(ThrottleParams { intensity })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ThrottleConfig {
        ThrottleConfig { threshold_bytes: 1000, max_intensity: 0.8 }
    }

    #[test]
    fn off_strategy_returns_none() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Off);
        assert!(ctrl.update(5000).is_none());
    }

    #[test]
    fn step_below_threshold() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Step);
        assert!(ctrl.update(999).is_none());
    }

    #[test]
    fn step_at_threshold() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Step);
        let params = ctrl.update(1000).unwrap();
        assert!((params.intensity - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn linear_below_threshold() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Linear);
        assert!(ctrl.update(500).is_none());
    }

    #[test]
    fn linear_at_max() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Linear);
        let params = ctrl.update(2000).unwrap();
        assert!((params.intensity - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn linear_midpoint() {
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Linear);
        let params = ctrl.update(1500).unwrap();
        assert!((params.intensity - 0.4).abs() < 0.01);
    }
}
