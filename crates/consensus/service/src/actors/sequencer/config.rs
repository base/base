//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::time::Duration;

use url::Url;

use super::ShadowFunding;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Configuration for the [`SequencerActor`].
///
/// The node's operating mode lives in [`NodeMode`]; this struct carries only the settings that
/// are independent of it.
///
/// [`SequencerActor`]: super::SequencerActor
/// [`NodeMode`]: crate::NodeMode
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerConfig {
    /// Whether or not the sequencer is enabled at startup.
    pub sequencer_stopped: bool,
    /// Whether or not the sequencer is in recovery mode.
    pub sequencer_recovery_mode: bool,
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
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            sequencer_stopped: false,
            sequencer_recovery_mode: false,
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
    use std::time::Duration;

    use super::{DEFAULT_CONDUCTOR_RPC_TIMEOUT, SequencerConfig};

    #[test]
    fn default_config_runs_an_unstopped_sequencer_without_a_conductor() {
        let config = SequencerConfig::default();

        assert!(!config.sequencer_stopped);
        assert!(!config.sequencer_recovery_mode);
        assert_eq!(config.conductor_rpc_url, None);
        assert!(!config.conductor_binary_commit);
        assert_eq!(config.conductor_rpc_timeout, DEFAULT_CONDUCTOR_RPC_TIMEOUT);
    }

    #[test]
    fn default_l1_rpc_timeout_is_five_hundred_milliseconds() {
        assert_eq!(SequencerConfig::default().l1_rpc_timeout, Duration::from_millis(500));
    }
}
