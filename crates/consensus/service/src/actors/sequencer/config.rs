//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::{fmt, num::NonZeroU64, str::FromStr, time::Duration};

use url::Url;

use super::ShadowFunding;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Sequencer synchronization source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SequencerSyncMode {
    /// Preserve the legacy sequencer behavior, where CL unsafe-block ingestion completes sync.
    #[default]
    Cl,
    /// Allow the sequencer to complete sync from the execution layer's canonical head.
    El,
}

impl SequencerSyncMode {
    /// Returns whether this mode completes sequencer sync from the execution layer.
    pub const fn is_el(self) -> bool {
        matches!(self, Self::El)
    }
}

impl fmt::Display for SequencerSyncMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cl => "cl",
            Self::El => "el",
        })
    }
}

impl FromStr for SequencerSyncMode {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "cl" => Ok(Self::Cl),
            "el" => Ok(Self::El),
            _ => Err(format!("expected `cl` or `el`, got `{raw}`")),
        }
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
    /// Number of private blocks to build per cycle when running as a shadow sequencer.
    ///
    /// When [`None`], the node runs as a normal sequencer.
    pub shadow_blocks_per_cycle: Option<NonZeroU64>,
    /// Optional account funding for the first private block of each shadow cycle.
    pub shadow_funding: Option<ShadowFunding>,
    /// Where the sequencer completes its initial chain sync from.
    pub sequencer_sync_mode: SequencerSyncMode,
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

    /// Returns whether shadow sequencer mode is enabled.
    pub const fn is_shadow_sequencer(&self) -> bool {
        self.shadow_blocks_per_cycle.is_some()
    }
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            sequencer_stopped: false,
            sequencer_recovery_mode: false,
            shadow_blocks_per_cycle: None,
            shadow_funding: None,
            sequencer_sync_mode: SequencerSyncMode::default(),
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
    use super::*;

    #[test]
    fn sync_mode_from_str_is_case_insensitive() {
        for raw in ["cl", "CL", "Cl", "cL"] {
            assert_eq!(SequencerSyncMode::from_str(raw), Ok(SequencerSyncMode::Cl));
        }
        for raw in ["el", "EL", "El", "eL"] {
            assert_eq!(SequencerSyncMode::from_str(raw), Ok(SequencerSyncMode::El));
        }
    }

    #[test]
    fn sync_mode_from_str_rejects_unknown_preserving_input() {
        let err = SequencerSyncMode::from_str("Xl").unwrap_err();
        assert_eq!(err, "expected `cl` or `el`, got `Xl`");
    }
}
