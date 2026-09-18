//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::{num::NonZeroU64, time::Duration};

use thiserror::Error;
use url::Url;

use super::ShadowFunding;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Sequencer operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequencerMode {
    /// Produces and publishes canonical unsafe blocks.
    Active,
    /// Produces private blocks before reconciling with the canonical chain.
    Shadow {
        /// Number of private blocks to build per cycle before reconciling.
        blocks_per_cycle: NonZeroU64,
    },
    /// Produces private blocks without canonical-chain ingress or payload publication.
    Isolated,
}

/// Errors returned when validating a [`SequencerConfig`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SequencerConfigError {
    /// An isolated sequencer was configured with a signing key.
    #[error("isolated sequencer must not configure a signing key")]
    IsolatedSigningKey,
    /// An isolated sequencer was configured with a conductor RPC endpoint.
    #[error("isolated sequencer must not configure a conductor RPC URL")]
    IsolatedConductorRpc,
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
    /// The sequencer operating mode.
    pub mode: SequencerMode,
    /// Optional account funding for the first private block of each shadow cycle.
    pub shadow_funding: Option<ShadowFunding>,
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
    /// Request timeout for L1 RPC calls on the sequencer block-production hot path.
    pub l1_rpc_timeout: Duration,
    /// Fixed offset into each subsecond slot at which the sealed payload is requested from
    /// the engine once Denim is active. Must agree with the builder-side transaction
    /// cutoff, which defaults from the same constant
    /// ([`base_protocol::DEFAULT_SEAL_OFFSET`]).
    pub seal_offset: Duration,
}

impl SequencerConfig {
    /// Maximum number of payloads retained for one shadow reconciliation cycle.
    pub const MAX_SHADOW_BLOCKS_PER_CYCLE: u64 = 300;
    /// Default request timeout for L1 RPC calls on the sequencer block-production hot path.
    pub const DEFAULT_L1_RPC_TIMEOUT: Duration = Duration::from_millis(500);

    /// Validates a fully constructed sequencer configuration against signing-key capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`SequencerConfigError::IsolatedSigningKey`] when an isolated sequencer has a
    /// signing key, or [`SequencerConfigError::IsolatedConductorRpc`] when it has a conductor RPC
    /// endpoint.
    pub fn validated(config: Self, has_signing_key: bool) -> Result<Self, SequencerConfigError> {
        if config.is_isolated() && has_signing_key {
            return Err(SequencerConfigError::IsolatedSigningKey);
        }
        if config.is_isolated() && config.conductor_rpc_url.is_some() {
            return Err(SequencerConfigError::IsolatedConductorRpc);
        }
        Ok(config)
    }

    /// Returns whether shadow sequencer mode is enabled.
    pub const fn is_shadow_sequencer(&self) -> bool {
        matches!(self.mode, SequencerMode::Shadow { .. })
    }

    /// Returns whether isolated sequencer mode is enabled.
    pub const fn is_isolated(&self) -> bool {
        matches!(self.mode, SequencerMode::Isolated)
    }

    /// Returns the configured shadow-cycle block count, or [`None`] outside shadow mode.
    pub const fn shadow_blocks_per_cycle(&self) -> Option<NonZeroU64> {
        match self.mode {
            SequencerMode::Shadow { blocks_per_cycle } => Some(blocks_per_cycle),
            SequencerMode::Active | SequencerMode::Isolated => None,
        }
    }
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            sequencer_stopped: false,
            sequencer_recovery_mode: false,
            mode: SequencerMode::Active,
            shadow_funding: None,
            conductor_rpc_url: None,
            conductor_binary_commit: false,
            conductor_rpc_timeout: DEFAULT_CONDUCTOR_RPC_TIMEOUT,
            l1_conf_delay: 0,
            l1_rpc_timeout: Self::DEFAULT_L1_RPC_TIMEOUT,
            seal_offset: base_protocol::DEFAULT_SEAL_OFFSET,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use url::Url;

    use super::{SequencerConfig, SequencerConfigError, SequencerMode};

    #[test]
    fn isolated_mode_is_reported() {
        let config = SequencerConfig { mode: SequencerMode::Isolated, ..Default::default() };

        assert!(config.is_isolated());
        assert!(!config.is_shadow_sequencer());
        assert_eq!(config.shadow_blocks_per_cycle(), None);
    }

    #[test]
    fn shadow_mode_reports_its_block_count() {
        let blocks_per_cycle = NonZeroU64::new(10).unwrap();
        let config = SequencerConfig {
            mode: SequencerMode::Shadow { blocks_per_cycle },
            ..Default::default()
        };

        assert!(config.is_shadow_sequencer());
        assert!(!config.is_isolated());
        assert_eq!(config.shadow_blocks_per_cycle(), Some(blocks_per_cycle));
    }

    #[test]
    fn validated_rejects_conductor_rpc_when_isolated() {
        let config = SequencerConfig {
            mode: SequencerMode::Isolated,
            conductor_rpc_url: Some(Url::parse("http://localhost:8545").unwrap()),
            ..Default::default()
        };

        let result = SequencerConfig::validated(config, false);

        assert_eq!(result, Err(SequencerConfigError::IsolatedConductorRpc));
    }

    #[test]
    fn validated_rejects_signing_key_when_isolated() {
        let config = SequencerConfig { mode: SequencerMode::Isolated, ..Default::default() };

        let result = SequencerConfig::validated(config, true);

        assert_eq!(result, Err(SequencerConfigError::IsolatedSigningKey));
    }
}
