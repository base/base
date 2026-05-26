//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::time::Duration;

use base_common_consensus::BASE_BLOCK_TIME_MILLIS;
use url::Url;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Default legacy L2 block time in seconds.
const DEFAULT_LEGACY_BLOCK_TIME: u64 = 2;

/// Sequencer wall-clock cadence configuration.
///
/// This configuration is intentionally separate from [`RollupConfig::block_time`], which remains
/// seconds-denominated for OP compatibility and legacy derivation semantics.
///
/// [`RollupConfig::block_time`]: base_common_genesis::RollupConfig::block_time
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerCadenceConfig {
    /// The wall-clock interval between sequencer block production attempts.
    pub block_interval: Duration,
    /// The initial delay before resetting the sequencer ticker after startup or recovery.
    pub initial_reset_backoff: Duration,
    /// The maximum number of in-flight block builds allowed for this cadence.
    pub max_pipeline_depth: usize,
    /// The leeway before an in-flight build is considered stale relative to the cadence.
    pub stale_build_leeway: Duration,
}

impl SequencerCadenceConfig {
    /// Returns a cadence matching the legacy seconds-denominated rollup block time.
    pub const fn from_legacy_block_time(block_time: u64) -> Self {
        let block_interval = Duration::from_secs(block_time);

        Self {
            block_interval,
            initial_reset_backoff: block_interval,
            max_pipeline_depth: 1,
            stale_build_leeway: Duration::ZERO,
        }
    }

    /// Returns the 200ms Beryl cadence without changing legacy rollup block-time semantics.
    pub const fn beryl_200ms() -> Self {
        let block_interval = Duration::from_millis(BASE_BLOCK_TIME_MILLIS as u64);

        Self {
            block_interval,
            initial_reset_backoff: block_interval,
            max_pipeline_depth: 1,
            stale_build_leeway: Duration::ZERO,
        }
    }
}

impl Default for SequencerCadenceConfig {
    fn default() -> Self {
        Self::from_legacy_block_time(DEFAULT_LEGACY_BLOCK_TIME)
    }
}

/// Configuration for the [`SequencerActor`].
///
/// [`SequencerActor`]: super::SequencerActor
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerConfig {
    /// Whether or not the sequencer is enabled at startup.
    pub sequencer_stopped: bool,
    /// Whether or not the sequencer is in recovery mode.
    pub sequencer_recovery_mode: bool,
    /// The [`Url`] for the conductor RPC endpoint. If [`Some`], enables the conductor service.
    pub conductor_rpc_url: Option<Url>,
    /// Use the conductor's SSZ-binary commit endpoint (`POST /commit-unsafe-payload`)
    /// instead of the JSON-RPC `conductor_commitUnsafePayload` method. Avoids the
    /// JSON encode/decode round trip on the leader's RPC handler — ~6–11x faster
    /// commit latency for typical mainnet payloads, and a prerequisite for blocks
    /// larger than the conductor's 5 `MiB` JSON-RPC body limit.
    ///
    /// Requires conductor with binary endpoint support
    /// (<https://github.com/base/optimism/pull/36>).
    pub conductor_binary_commit: bool,
    /// Request timeout for conductor RPC calls (both JSON-RPC and binary commit).
    pub conductor_rpc_timeout: Duration,
    /// The confirmation delay for the sequencer.
    pub l1_conf_delay: u64,
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            sequencer_stopped: false,
            sequencer_recovery_mode: false,
            conductor_rpc_url: None,
            conductor_binary_commit: false,
            conductor_rpc_timeout: DEFAULT_CONDUCTOR_RPC_TIMEOUT,
            l1_conf_delay: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BASE_BLOCK_TIME_MILLIS, DEFAULT_LEGACY_BLOCK_TIME, SequencerCadenceConfig};

    #[test]
    fn legacy_cadence_uses_seconds_denominated_rollup_block_time() {
        let cadence = SequencerCadenceConfig::from_legacy_block_time(2);

        assert_eq!(cadence.block_interval, Duration::from_secs(2));
        assert_eq!(cadence.initial_reset_backoff, Duration::from_secs(2));
        assert_eq!(cadence.max_pipeline_depth, 1);
        assert_eq!(cadence.stale_build_leeway, Duration::ZERO);
    }

    #[test]
    fn default_cadence_preserves_legacy_two_second_behavior() {
        assert_eq!(
            SequencerCadenceConfig::default(),
            SequencerCadenceConfig::from_legacy_block_time(DEFAULT_LEGACY_BLOCK_TIME)
        );
    }

    #[test]
    fn beryl_cadence_uses_200ms_slot_constant() {
        let cadence = SequencerCadenceConfig::beryl_200ms();

        assert_eq!(
            cadence.block_interval,
            Duration::from_millis(u64::from(BASE_BLOCK_TIME_MILLIS))
        );
        assert_eq!(cadence.initial_reset_backoff, cadence.block_interval);
        assert_eq!(cadence.max_pipeline_depth, 1);
        assert_eq!(cadence.stale_build_leeway, Duration::ZERO);
    }
}
