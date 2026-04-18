//! Throttle controller for DA backlog management.

/// Configuration for the throttle controller.
///
/// Defaults match the op-batcher reference implementation:
/// 1 MB threshold, full intensity, linear strategy.
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
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    derive_more::Display,
    derive_more::FromStr,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThrottleStrategy {
    /// No throttling.
    #[display("off")]
    Off,
    /// Step function: either 0 or `max_intensity` when above threshold.
    #[display("step")]
    Step,
    /// Linear interpolation between 0 and `max_intensity` based on backlog.
    #[display("linear")]
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

    /// Returns a [`ThrottleController`] with [`ThrottleStrategy::Off`] that never throttles.
    ///
    /// Useful in tests and configurations where DA backlog throttling should be disabled.
    pub fn noop() -> Self {
        Self::new(
            ThrottleConfig { threshold_bytes: 0, max_intensity: 0.0, ..Default::default() },
            ThrottleStrategy::Off,
        )
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

/// Wraps a [`ThrottleController`] and a [`ThrottleClient`] with a dedup cache
/// to avoid redundant RPC calls when DA limits have not changed.
#[derive(Debug)]
pub struct DaThrottle<TC: crate::ThrottleClient> {
    controller: ThrottleController,
    client: TC,
    last_applied: Option<(u64, u64)>,
}

impl<TC: crate::ThrottleClient> DaThrottle<TC> {
    /// Create a new [`DaThrottle`].
    pub const fn new(controller: ThrottleController, client: TC) -> Self {
        Self { controller, client, last_applied: None }
    }

    /// Compute new DA limits from `backlog_bytes` and push them to the client
    /// only when they differ from the last applied limits.
    ///
    /// Returns `true` if throttling is currently active (intensity > 0).
    /// Logs throttle on/off transitions.
    pub async fn apply(&mut self, backlog_bytes: u64) -> bool {
        let throttle_params = self.controller.update(backlog_bytes);
        let is_throttling = throttle_params.as_ref().is_some_and(ThrottleParams::is_throttling);

        let (max_tx_size, max_block_size) = throttle_params.as_ref().map_or_else(
            || {
                (
                    self.controller.config().tx_size_upper_limit,
                    self.controller.config().block_size_upper_limit,
                )
            },
            |p| (p.max_tx_size, p.max_block_size),
        );

        let new_limits = (max_tx_size, max_block_size);
        if self.last_applied == Some(new_limits) {
            return is_throttling;
        }

        if let Err(e) = self.client.set_max_da_size(max_tx_size, max_block_size).await {
            tracing::warn!(error = %e, "failed to apply DA size limits to block builder");
        } else {
            if is_throttling {
                tracing::info!(
                    intensity = throttle_params.as_ref().unwrap().intensity,
                    max_block_size,
                    max_tx_size,
                    "DA throttle activated"
                );
            } else {
                tracing::info!(
                    max_block_size,
                    max_tx_size,
                    "DA throttle deactivated, limits reset"
                );
            }
            self.last_applied = Some(new_limits);
        }
        is_throttling
    }

    /// Compute a point-in-time snapshot of the current throttle state.
    ///
    /// Params are derived from `backlog_bytes` on demand; no additional
    /// state is stored in `DaThrottle` beyond what is already tracked.
    pub fn snapshot(&self, backlog_bytes: u64) -> ThrottleInfo {
        let params = self.controller.update(backlog_bytes);
        let config = self.controller.config();
        ThrottleInfo {
            strategy: self.controller.strategy().clone(),
            threshold_bytes: config.threshold_bytes,
            max_intensity: config.max_intensity,
            current_intensity: params.map_or(0.0, |p| p.intensity),
            max_block_size: params.map_or(config.block_size_upper_limit, |p| p.max_block_size),
            max_tx_size: params.map_or(config.tx_size_upper_limit, |p| p.max_tx_size),
        }
    }

    /// Replace the controller and clear the dedup cache so new limits are
    /// applied unconditionally on the next driver iteration.
    pub const fn set_controller(&mut self, controller: ThrottleController) {
        self.controller = controller;
        self.last_applied = None;
    }

    /// Clear the dedup cache so the current limits are re-sent to the client
    /// on the next driver iteration even if they have not changed.
    pub const fn reset(&mut self) {
        self.last_applied = None;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn test_config() -> ThrottleConfig {
        ThrottleConfig { threshold_bytes: 1000, max_intensity: 0.8, ..Default::default() }
    }

    /// Verifies `ThrottleController::update` returns the correct result for each
    /// strategy and backlog combination.
    ///
    /// `expected_intensity` is `None` when no throttling should be applied, or
    /// `Some(intensity)` when throttling must be active with that intensity value.
    #[rstest]
    #[case::off_always_none(ThrottleStrategy::Off, 5000, None)]
    #[case::step_below_threshold(ThrottleStrategy::Step, 999, None)]
    #[case::step_at_threshold(ThrottleStrategy::Step, 1000, Some(0.8))]
    #[case::linear_below_threshold(ThrottleStrategy::Linear, 500, None)]
    // At exactly the threshold, excess = 0 → intensity = 0.0 → must return None,
    // not Some with zero intensity (which would trigger a spurious log on startup).
    #[case::linear_at_threshold(ThrottleStrategy::Linear, 1000, None)]
    #[case::linear_at_max(ThrottleStrategy::Linear, 2000, Some(0.8))]
    #[case::linear_midpoint(ThrottleStrategy::Linear, 1500, Some(0.4))]
    fn test_update(
        #[case] strategy: ThrottleStrategy,
        #[case] da_backlog_bytes: u64,
        #[case] expected_intensity: Option<f64>,
    ) {
        let ctrl = ThrottleController::new(test_config(), strategy);
        let result = ctrl.update(da_backlog_bytes);
        match expected_intensity {
            None => assert!(result.is_none()),
            Some(expected) => {
                let params = result.expect("expected Some result");
                assert!(
                    (params.intensity - expected).abs() < 0.01,
                    "expected intensity {expected}, got {}",
                    params.intensity
                );
            }
        }
    }

    #[test]
    fn strategy_display_parse_roundtrip() {
        for (input, expected) in [
            (ThrottleStrategy::Off, "off"),
            (ThrottleStrategy::Step, "step"),
            (ThrottleStrategy::Linear, "linear"),
        ] {
            assert_eq!(input.to_string(), expected);
            assert_eq!(expected.parse::<ThrottleStrategy>().unwrap(), input);
        }
    }

    #[test]
    fn strategy_parse_rejects_invalid() {
        assert!("foo".parse::<ThrottleStrategy>().is_err());
        assert!("".parse::<ThrottleStrategy>().is_err());
    }
}
