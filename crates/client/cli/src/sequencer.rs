//! Sequencer CLI Flags
//!
//! These are based on sequencer flags from the [`op-node`][op-node] CLI.
//!
//! [op-node]: https://github.com/ethereum-optimism/optimism/blob/develop/op-node/flags/flags.go#L233-L265

use std::time::Duration;

use base_consensus_node::SequencerConfig;
use url::Url;

/// Sequencer CLI Flags
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencerArgs {
    /// Initialize the sequencer in a stopped state. The sequencer can be started using the
    /// `admin_startSequencer` RPC.
    pub stopped: bool,

    /// Maximum number of L2 blocks for restricting the distance between L2 safe and unsafe.
    /// Disabled if 0.
    pub max_safe_lag: u64,

    /// Number of L1 blocks to keep distance from the L1 head as a sequencer for picking an L1
    /// origin.
    pub l1_confs: u64,

    /// Forces the sequencer to strictly prepare the next L1 origin and create empty L2 blocks
    pub recover: bool,

    /// Conductor service rpc endpoint. Providing this value will enable the conductor service.
    pub conductor_rpc: Option<Url>,

    /// Conductor service rpc timeout.
    pub conductor_rpc_timeout: Duration,
}

impl Default for SequencerArgs {
    fn default() -> Self {
        Self {
            stopped: false,
            max_safe_lag: 0,
            l1_confs: 4,
            recover: false,
            conductor_rpc: None,
            conductor_rpc_timeout: Duration::from_secs(1),
        }
    }
}

impl SequencerArgs {
    /// Creates a [`SequencerConfig`] from the [`SequencerArgs`].
    pub fn config(&self) -> SequencerConfig {
        SequencerConfig {
            sequencer_stopped: self.stopped,
            sequencer_recovery_mode: self.recover,
            conductor_rpc_url: self.conductor_rpc.clone(),
            l1_conf_delay: self.l1_confs,
        }
    }
}
