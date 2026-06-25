//! Configuration for the [`SequencerActor`].
//!
//! [`SequencerActor`]: super::SequencerActor

use std::time::Duration;

use url::Url;

/// Default conductor RPC timeout (1 second), matching the CLI default.
const DEFAULT_CONDUCTOR_RPC_TIMEOUT: Duration = Duration::from_secs(1);

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
