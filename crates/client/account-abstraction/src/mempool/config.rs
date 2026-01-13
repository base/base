//! Mempool Configuration
//!
//! Configuration options for the UserOperation mempool.

use std::time::Duration;

/// Configuration for the UserOperation mempool
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Maximum UserOps per sender in the mempool
    ///
    /// Prevents a single sender from consuming too much mempool space.
    /// Default: 4
    pub max_ops_per_sender: usize,

    /// Maximum UserOps per unstaked paymaster
    ///
    /// Unstaked paymasters are limited to prevent spam.
    /// Staked paymasters have no limit.
    /// Default: 4
    pub max_ops_per_unstaked_paymaster: usize,

    /// Maximum UserOps per unstaked factory
    ///
    /// Unstaked factories are limited to prevent spam.
    /// Default: 4
    pub max_ops_per_unstaked_factory: usize,

    /// Maximum total UserOps per entrypoint pool
    ///
    /// When this limit is reached, lowest gas price UserOps are evicted.
    /// Default: 10,000
    pub max_pool_size_per_entrypoint: usize,

    /// Maximum time a UserOp can stay in the mempool
    ///
    /// UserOps are evicted after this duration regardless of their validUntil.
    /// Default: 1 hour
    pub max_time_in_pool: Duration,

    /// Minimum gas price bump required for replacement (percentage)
    ///
    /// A new UserOp with the same (sender, nonce) must have gas price
    /// at least this percentage higher to replace the existing one.
    /// Default: 10 (10%)
    pub replacement_gas_bump_percent: u64,

    /// Throttle threshold ratio (opsIncluded / opsSeen)
    ///
    /// When an entity's inclusion ratio falls below this threshold,
    /// they enter throttled status and only 1 in 10 of their UserOps are accepted.
    /// Default: 0.1 (10%)
    pub throttle_threshold: f64,

    /// Ban threshold ratio (opsFailed / opsSeen)
    ///
    /// When an entity's failure ratio exceeds this threshold,
    /// they are banned and all their UserOps are rejected.
    /// Default: 0.5 (50%)
    pub ban_threshold: f64,

    /// Reputation decay interval
    ///
    /// Entity reputation counters are decayed at this interval.
    /// Default: 1 hour
    pub reputation_decay_interval: Duration,

    /// Reputation decay factor
    ///
    /// Counters are multiplied by this factor at each decay interval.
    /// Default: 0.9 (10% decay)
    pub reputation_decay_factor: f64,

    /// Minimum operations seen before reputation rules apply
    ///
    /// New entities get a grace period before throttling/banning.
    /// Default: 10
    pub min_ops_for_reputation: u64,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_ops_per_sender: 4,
            max_ops_per_unstaked_paymaster: 4,
            max_ops_per_unstaked_factory: 4,
            max_pool_size_per_entrypoint: 10_000,
            max_time_in_pool: Duration::from_secs(3600), // 1 hour
            replacement_gas_bump_percent: 10,
            throttle_threshold: 0.1,
            ban_threshold: 0.5,
            reputation_decay_interval: Duration::from_secs(3600), // 1 hour
            reputation_decay_factor: 0.9,
            min_ops_for_reputation: 10,
        }
    }
}

impl MempoolConfig {
    /// Create a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum UserOps per sender
    pub fn with_max_ops_per_sender(mut self, max: usize) -> Self {
        self.max_ops_per_sender = max;
        self
    }

    /// Set maximum UserOps per unstaked paymaster
    pub fn with_max_ops_per_unstaked_paymaster(mut self, max: usize) -> Self {
        self.max_ops_per_unstaked_paymaster = max;
        self
    }

    /// Set maximum pool size per entrypoint
    pub fn with_max_pool_size(mut self, max: usize) -> Self {
        self.max_pool_size_per_entrypoint = max;
        self
    }

    /// Set maximum time in pool
    pub fn with_max_time_in_pool(mut self, duration: Duration) -> Self {
        self.max_time_in_pool = duration;
        self
    }

    /// Set replacement gas bump percentage
    pub fn with_replacement_gas_bump(mut self, percent: u64) -> Self {
        self.replacement_gas_bump_percent = percent;
        self
    }

    /// Calculate required gas price for replacement
    ///
    /// Returns the minimum gas price needed to replace a UserOp with the given current price.
    pub fn required_replacement_gas_price(&self, current_gas_price: u128) -> u128 {
        let bump = current_gas_price * self.replacement_gas_bump_percent as u128 / 100;
        current_gas_price.saturating_add(bump)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MempoolConfig::default();
        assert_eq!(config.max_ops_per_sender, 4);
        assert_eq!(config.max_pool_size_per_entrypoint, 10_000);
        assert_eq!(config.replacement_gas_bump_percent, 10);
    }

    #[test]
    fn test_replacement_gas_price() {
        let config = MempoolConfig::default();

        // 10% bump on 100 gwei = 110 gwei
        let required = config.required_replacement_gas_price(100_000_000_000);
        assert_eq!(required, 110_000_000_000);

        // 10% bump on 1 gwei = 1.1 gwei
        let required = config.required_replacement_gas_price(1_000_000_000);
        assert_eq!(required, 1_100_000_000);
    }

    #[test]
    fn test_builder_pattern() {
        let config = MempoolConfig::new()
            .with_max_ops_per_sender(8)
            .with_max_pool_size(5_000)
            .with_replacement_gas_bump(20);

        assert_eq!(config.max_ops_per_sender, 8);
        assert_eq!(config.max_pool_size_per_entrypoint, 5_000);
        assert_eq!(config.replacement_gas_bump_percent, 20);
    }
}
