//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::time::Duration;

use url::Url;

pub(super) const BASE_BLOCK_TIME_MILLIS: u64 = 200;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Default legacy block time (2 seconds), used prior to Zombie activation.
const DEFAULT_LEGACY_BLOCK_TIME: u64 = 2;

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

/// Block production cadence for the [`SequencerActor`].
///
/// Controls how frequently the sequencer builds blocks. Prior to Zombie the
/// cadence tracks the legacy `block_time` (seconds); once Zombie is active the
/// sequencer runs on the 200ms sub-second cadence.
///
/// [`SequencerActor`]: super::SequencerActor
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerCadenceConfig {
    /// The interval between consecutive block builds.
    pub block_interval: Duration,
}

impl SequencerCadenceConfig {
    /// Builds a cadence from the legacy `block_time` expressed in whole seconds.
    pub const fn from_legacy_block_time(block_time: u64) -> Self {
        Self { block_interval: Duration::from_secs(block_time) }
    }

    /// Builds the 200ms cadence used once the Zombie upgrade is active.
    pub const fn zombie_200ms() -> Self {
        Self { block_interval: Duration::from_millis(BASE_BLOCK_TIME_MILLIS as u64) }
    }
}

impl Default for SequencerCadenceConfig {
    fn default() -> Self {
        Self::from_legacy_block_time(DEFAULT_LEGACY_BLOCK_TIME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_cadence_uses_seconds() {
        let cadence = SequencerCadenceConfig::from_legacy_block_time(2);
        assert_eq!(cadence.block_interval, Duration::from_secs(2));
    }

    #[test]
    fn zombie_cadence_is_200ms() {
        let cadence = SequencerCadenceConfig::zombie_200ms();
        assert_eq!(cadence.block_interval, Duration::from_millis(200));
    }

    #[test]
    fn default_cadence_matches_legacy_two_seconds() {
        assert_eq!(
            SequencerCadenceConfig::default(),
            SequencerCadenceConfig::from_legacy_block_time(DEFAULT_LEGACY_BLOCK_TIME)
        );
    }
}
