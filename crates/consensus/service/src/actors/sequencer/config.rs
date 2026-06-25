//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::{fmt, str::FromStr, time::Duration};

use url::Url;

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
        match raw {
            "cl" | "CL" => Ok(Self::Cl),
            "el" | "EL" => Ok(Self::El),
            other => Err(format!("expected `cl` or `el`, got `{other}`")),
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
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            sequencer_stopped: false,
            sequencer_recovery_mode: false,
            sequencer_sync_mode: SequencerSyncMode::default(),
            conductor_rpc_url: None,
            conductor_binary_commit: false,
            conductor_rpc_timeout: DEFAULT_CONDUCTOR_RPC_TIMEOUT,
            l1_conf_delay: 0,
        }
    }
}
