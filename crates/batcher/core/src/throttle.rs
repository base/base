//! Throttle controller for DA backlog management.

use tokio::sync::watch;
use tracing::info;

/// Configuration for the throttle controller.
///
/// Must pass [`validate`](Self::validate) before use because the block builder reads a DA
/// limit of 0 as no limit at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

impl ThrottleConfig {
    /// Checks that every limit the throttle can produce stays within its `[lower, upper]`
    /// range and above zero.
    pub fn validate(&self) -> Result<(), ThrottleConfigError> {
        if !(0.0..=1.0).contains(&self.max_intensity) {
            return Err(ThrottleConfigError::MaxIntensityOutOfRange {
                max_intensity: self.max_intensity,
            });
        }
        Self::validate_limits(
            "block_size",
            self.block_size_lower_limit,
            self.block_size_upper_limit,
        )?;
        Self::validate_limits("tx_size", self.tx_size_lower_limit, self.tx_size_upper_limit)
    }

    /// Checks one lower and upper limit pair. Full intensity sends the lower limit.
    const fn validate_limits(
        name: &'static str,
        lower: u64,
        upper: u64,
    ) -> Result<(), ThrottleConfigError> {
        if lower == 0 {
            return Err(ThrottleConfigError::ZeroLowerLimit { name });
        }
        if lower > upper {
            return Err(ThrottleConfigError::LowerAboveUpper { name, lower, upper });
        }
        Ok(())
    }
}

/// Errors returned when validating [`ThrottleConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ThrottleConfigError {
    /// `max_intensity` is outside `[0, 1]`, or `NaN`, so throttling would push a limit outside
    /// its `[lower, upper]` range.
    #[error("max_intensity ({max_intensity}) must be within [0, 1]")]
    MaxIntensityOutOfRange {
        /// The configured maximum intensity.
        max_intensity: f64,
    },
    /// A lower limit is 0, which full intensity sends as is.
    #[error("{name}_lower_limit must be greater than zero")]
    ZeroLowerLimit {
        /// The limit pair, `block_size` or `tx_size`.
        name: &'static str,
    },
    /// A lower limit is above its upper limit, so throttling would raise the limit.
    #[error("{name}_lower_limit ({lower}) must not exceed {name}_upper_limit ({upper})")]
    LowerAboveUpper {
        /// The limit pair, `block_size` or `tx_size`.
        name: &'static str,
        /// The configured lower limit.
        lower: u64,
        /// The configured upper limit.
        upper: u64,
    },
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThrottleStrategy {
    /// No throttling.
    Off,
    /// Step function: 0 below the threshold, `max_intensity` from the threshold on.
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

    /// Returns a controller with [`ThrottleStrategy::Off`], which never throttles.
    pub fn disabled() -> Self {
        Self::new(ThrottleConfig::default(), ThrottleStrategy::Off)
    }

    /// Returns a reference to the throttle configuration.
    pub const fn config(&self) -> &ThrottleConfig {
        &self.config
    }

    /// Returns the active throttle strategy.
    pub const fn strategy(&self) -> &ThrottleStrategy {
        &self.strategy
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

    /// Returns the DA limits to apply for `params`, which are the upper limits while not
    /// throttling.
    pub const fn limits(&self, params: Option<&ThrottleParams>) -> DaLimits {
        match params {
            Some(params) => {
                DaLimits { max_tx_size: params.max_tx_size, max_block_size: params.max_block_size }
            }
            None => DaLimits {
                max_tx_size: self.config.tx_size_upper_limit,
                max_block_size: self.config.block_size_upper_limit,
            },
        }
    }

    /// Update with current DA backlog bytes.
    ///
    /// Returns [`ThrottleParams`] if throttling should be applied, or `None`
    /// if the backlog is below the threshold, the linear intensity is zero or the
    /// strategy is [`ThrottleStrategy::Off`].
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
                // Intensity grows linearly from 0 at the threshold to max_intensity at twice
                // the threshold, and stays there above it.
                let excess = da_backlog_bytes - self.config.threshold_bytes;
                let range = self.config.threshold_bytes.max(1);
                let ratio = (excess as f64 / range as f64).min(1.0);
                let intensity = ratio * self.config.max_intensity;
                // A zero intensity, at exactly the threshold or with a zero `max_intensity`, is no
                // throttling, as below the threshold.
                if intensity == 0.0 {
                    return None;
                }
                let (max_block_size, max_tx_size) = self.compute_limits(intensity);
                Some(ThrottleParams { intensity, max_block_size, max_tx_size })
            }
        }
    }
}

/// Point-in-time snapshot of throttle controller state.
///
/// Returned by [`DaThrottle::snapshot`] and serialised directly as the
/// `admin_getThrottleController` JSON-RPC response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThrottleInfo {
    /// Active throttle strategy.
    pub strategy: ThrottleStrategy,
    /// Backlog threshold in bytes at which throttling activates.
    pub threshold_bytes: u64,
    /// Maximum throttle intensity (0.0 to 1.0).
    pub max_intensity: f64,
    /// Current throttle intensity (0.0 when not throttling).
    pub current_intensity: f64,
    /// Current maximum DA bytes allowed per block.
    pub max_block_size: u64,
    /// Current maximum DA bytes allowed per transaction.
    pub max_tx_size: u64,
}

/// DA size limits for the block builders, pushed through `miner_setMaxDASize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaLimits {
    /// Maximum DA bytes allowed per transaction.
    pub max_tx_size: u64,
    /// Maximum DA bytes allowed per block.
    pub max_block_size: u64,
}

/// Turns the DA backlog into the limits the block builders should apply, and publishes them.
///
/// The limits are published on a [`watch`] channel that keeps only the latest value, so a
/// subscriber that falls behind pushes the current limits and never a stale one. Pushing them to
/// the block builders is up to the subscribers, see [`subscribe`](Self::subscribe).
#[derive(Debug)]
pub struct DaThrottle {
    controller: ThrottleController,
    limits: watch::Sender<DaLimits>,
}

impl DaThrottle {
    /// Creates a throttle publishing the limits of `controller`, starting from its limits for an
    /// empty backlog.
    pub fn new(controller: ThrottleController) -> Self {
        let limits = watch::Sender::new(controller.limits(controller.update(0).as_ref()));
        Self { controller, limits }
    }

    /// Returns a receiver of the published limits. It starts with the current limits marked as
    /// seen, and is notified of every later publication.
    pub fn subscribe(&self) -> watch::Receiver<DaLimits> {
        self.limits.subscribe()
    }

    /// Computes the limits for `backlog_bytes` and publishes them when they change.
    ///
    /// Returns `true` if throttling is currently active (intensity > 0).
    pub fn apply(&mut self, backlog_bytes: u64) -> bool {
        let params = self.controller.update(backlog_bytes);
        let limits = self.controller.limits(params.as_ref());

        let published = self.limits.send_if_modified(|current| {
            let changed = *current != limits;
            *current = limits;
            changed
        });
        if published {
            info!(
                intensity = params.map_or(0.0, |p| p.intensity),
                max_block_size = limits.max_block_size,
                max_tx_size = limits.max_tx_size,
                "DA limits published"
            );
        }

        params.as_ref().is_some_and(ThrottleParams::is_throttling)
    }

    /// Compute a point-in-time snapshot of the current throttle state.
    ///
    /// Params are derived from `backlog_bytes` on demand; no additional
    /// state is stored in `DaThrottle` beyond what is already tracked.
    pub fn snapshot(&self, backlog_bytes: u64) -> ThrottleInfo {
        let params = self.controller.update(backlog_bytes);
        let config = self.controller.config();
        let limits = self.controller.limits(params.as_ref());
        ThrottleInfo {
            strategy: self.controller.strategy().clone(),
            threshold_bytes: config.threshold_bytes,
            max_intensity: config.max_intensity,
            current_intensity: params.map_or(0.0, |p| p.intensity),
            max_block_size: limits.max_block_size,
            max_tx_size: limits.max_tx_size,
        }
    }

    /// Replaces the controller. Its limits are published on the next driver iteration.
    pub const fn set_controller(&mut self, controller: ThrottleController) {
        self.controller = controller;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The intensity and DA limits each strategy applies to a backlog, `None` meaning no
    /// throttling.
    #[rstest]
    #[case::off(ThrottleStrategy::Off, 5000, None)]
    #[case::step_below_threshold(ThrottleStrategy::Step, 999, None)]
    #[case::step_at_threshold(ThrottleStrategy::Step, 1000, Some((0.8, 27_600, 4_120)))]
    #[case::linear_below_threshold(ThrottleStrategy::Linear, 500, None)]
    #[case::linear_at_threshold(ThrottleStrategy::Linear, 1000, None)]
    #[case::linear_midpoint(ThrottleStrategy::Linear, 1500, Some((0.4, 78_800, 12_060)))]
    #[case::linear_at_twice_the_threshold(
        ThrottleStrategy::Linear,
        2000,
        Some((0.8, 27_600, 4_120))
    )]
    #[case::linear_above_twice_the_threshold(
        ThrottleStrategy::Linear,
        5000,
        Some((0.8, 27_600, 4_120))
    )]
    fn update_throttles_by_strategy_and_backlog(
        #[case] strategy: ThrottleStrategy,
        #[case] da_backlog_bytes: u64,
        #[case] expected: Option<(f64, u64, u64)>,
    ) {
        let config =
            ThrottleConfig { threshold_bytes: 1000, max_intensity: 0.8, ..Default::default() };
        let controller = ThrottleController::new(config, strategy);

        let params = controller.update(da_backlog_bytes);

        assert_eq!(
            params.map(|params| (params.intensity, params.max_block_size, params.max_tx_size)),
            expected
        );
    }

    /// A config is valid when the intensity is within [0, 1], both lower limits are above zero
    /// and each lower limit is at most its upper limit. Otherwise the error names the broken rule.
    #[rstest]
    #[case::default(ThrottleConfig::default(), Ok(()))]
    #[case::equal_limits(
        ThrottleConfig { block_size_lower_limit: 130_000, ..Default::default() },
        Ok(())
    )]
    #[case::intensity_above_one(
        ThrottleConfig { max_intensity: 1.5, ..Default::default() },
        Err("max_intensity (1.5) must be within [0, 1]")
    )]
    #[case::negative_intensity(
        ThrottleConfig { max_intensity: -0.1, ..Default::default() },
        Err("max_intensity (-0.1) must be within [0, 1]")
    )]
    #[case::nan_intensity(
        ThrottleConfig { max_intensity: f64::NAN, ..Default::default() },
        Err("max_intensity (NaN) must be within [0, 1]")
    )]
    #[case::zero_block_lower(
        ThrottleConfig { block_size_lower_limit: 0, ..Default::default() },
        Err("block_size_lower_limit must be greater than zero")
    )]
    #[case::zero_tx_lower(
        ThrottleConfig { tx_size_lower_limit: 0, ..Default::default() },
        Err("tx_size_lower_limit must be greater than zero")
    )]
    #[case::block_lower_above_upper(
        ThrottleConfig { block_size_lower_limit: 130_001, ..Default::default() },
        Err("block_size_lower_limit (130001) must not exceed block_size_upper_limit (130000)")
    )]
    #[case::tx_lower_above_upper(
        ThrottleConfig { tx_size_lower_limit: 20_001, ..Default::default() },
        Err("tx_size_lower_limit (20001) must not exceed tx_size_upper_limit (20000)")
    )]
    fn validate_accepts_only_limits_within_range(
        #[case] config: ThrottleConfig,
        #[case] expected: Result<(), &str>,
    ) {
        assert_eq!(
            config.validate().map_err(|error| error.to_string()),
            expected.map_err(String::from)
        );
    }
}
