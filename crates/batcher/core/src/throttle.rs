//! Throttle controller for DA backlog management.

/// Configuration for the throttle controller.
///
/// Defaults match the op-batcher reference implementation:
/// 1 MB threshold, full intensity, linear strategy.
#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    /// Backlog threshold in bytes at which throttling activates.
    /// Default: 1,000,000 bytes (1 MB).
    pub threshold_bytes: u64,
    /// Maximum throttle intensity (0.0 to 1.0).
    /// Default: 1.0 (full throttle at 2× threshold for [`ThrottleStrategy::Linear`]).
    pub max_intensity: f64,
    /// Maximum block DA bytes allowed at full throttle intensity.
    /// Default: 2,000 bytes.
    pub block_size_lower_limit: u64,
    /// Maximum block DA bytes allowed when not throttling.
    /// Default: 130,000 bytes.
    pub block_size_upper_limit: u64,
    /// Maximum transaction DA bytes allowed at full throttle intensity.
    /// Default: 150 bytes.
    pub tx_size_lower_limit: u64,
    /// Maximum transaction DA bytes allowed when not throttling.
    /// Default: 20,000 bytes.
    pub tx_size_upper_limit: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        // Match op-batcher's defaults.
        Self {
            threshold_bytes: 1_000_000,
            max_intensity: 1.0,
            block_size_lower_limit: 2_000,
            block_size_upper_limit: 130_000,
            tx_size_lower_limit: 150,
            tx_size_upper_limit: 20_000,
        }
    }
}

/// Parameters to apply when throttling is active.
#[derive(Debug, Clone, Copy)]
pub struct ThrottleParams {
    /// Fraction of normal submission rate to apply (0.0 to 1.0).
    pub intensity: f64,
    /// Maximum DA bytes allowed per block at the current throttle intensity.
    pub max_block_size: u64,
    /// Maximum DA bytes allowed per transaction at the current throttle intensity.
    pub max_tx_size: u64,
}

impl ThrottleParams {
    /// Returns `true` if throttling is actively reducing DA limits.
    pub fn is_throttling(&self) -> bool {
        self.intensity > 0.0
    }
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

    /// Returns a reference to the throttle configuration.
    pub const fn config(&self) -> &ThrottleConfig {
        &self.config
    }

    /// Compute DA size limits from the given intensity.
    fn compute_limits(&self, intensity: f64) -> (u64, u64) {
        let block_range =
            self.config.block_size_upper_limit as f64 - self.config.block_size_lower_limit as f64;
        let tx_range =
            self.config.tx_size_upper_limit as f64 - self.config.tx_size_lower_limit as f64;

        let max_block_size =
            (self.config.block_size_upper_limit as f64 - intensity * block_range).round() as u64;
        let max_tx_size =
            (self.config.tx_size_upper_limit as f64 - intensity * tx_range).round() as u64;

        (max_block_size, max_tx_size)
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
                    let intensity = self.config.max_intensity;
                    let (max_block_size, max_tx_size) = self.compute_limits(intensity);
                    Some(ThrottleParams { intensity, max_block_size, max_tx_size })
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
                // At exactly the threshold excess is zero, so intensity is 0.0 and
                // limits would be the same as unthrottled — return None to avoid a
                // spurious "DA throttle deactivated" log entry on startup.
                if intensity == 0.0 {
                    return None;
                }
                let (max_block_size, max_tx_size) = self.compute_limits(intensity);
                Some(ThrottleParams { intensity, max_block_size, max_tx_size })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ThrottleConfig {
        ThrottleConfig { threshold_bytes: 1000, max_intensity: 0.8, ..Default::default() }
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
    fn linear_at_threshold_returns_none() {
        // At exactly the threshold, excess = 0 → intensity = 0.0 → must return None,
        // not Some with zero intensity (which would trigger a spurious log on startup).
        let ctrl = ThrottleController::new(test_config(), ThrottleStrategy::Linear);
        assert!(ctrl.update(1000).is_none());
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
