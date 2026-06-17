//! Sequencer consensus-control CLI flags.

use std::{num::ParseIntError, time::Duration};

use base_consensus_node::SequencerConfig;
use clap::Parser;
use url::Url;

/// Sequencer consensus-control CLI flags.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
pub struct SequencerArgs {
    /// Initialize the sequencer in a stopped state. The sequencer can be started using the
    /// `admin_startSequencer` RPC.
    #[arg(
        long = "sequencer.stopped",
        default_value = "false",
        env = "BASE_NODE_SEQUENCER_STOPPED"
    )]
    pub stopped: bool,

    /// Maximum number of L2 blocks for restricting the distance between L2 safe and unsafe.
    ///
    /// Currently accepted by the CLI but not enforced by the sequencer runtime. Disabled if 0.
    #[arg(
        long = "sequencer.max-safe-lag",
        default_value = "0",
        env = "BASE_NODE_SEQUENCER_MAX_SAFE_LAG"
    )]
    pub max_safe_lag: u64,

    /// Number of L1 blocks to keep distance from the L1 head as a sequencer when picking an L1
    /// origin.
    #[arg(long = "sequencer.l1-confs", default_value = "4", env = "BASE_NODE_SEQUENCER_L1_CONFS")]
    pub l1_confs: u64,

    /// Force the sequencer to strictly prepare the next L1 origin and create empty L2 blocks.
    #[arg(
        long = "sequencer.recover",
        default_value = "false",
        env = "BASE_NODE_SEQUENCER_RECOVER"
    )]
    pub recover: bool,

    /// Conductor service RPC endpoint. Providing this value enables the conductor service.
    #[arg(long = "conductor.rpc", env = "BASE_NODE_CONDUCTOR_RPC")]
    pub conductor_rpc: Option<Url>,

    /// Conductor service RPC timeout.
    #[arg(
        long = "conductor.rpc.timeout",
        default_value = "1",
        env = "BASE_NODE_CONDUCTOR_RPC_TIMEOUT",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {Ok(Duration::from_secs(arg.parse()?))}
    )]
    pub conductor_rpc_timeout: Duration,

    /// Use the conductor's SSZ-binary commit-unsafe-payload endpoint instead of JSON-RPC.
    /// Avoids JSON encode/decode (~6-11x faster on the leader RPC handler for typical
    /// mainnet payloads). Requires conductor with binary endpoint support.
    #[arg(
        long = "conductor.binary-commit",
        default_value = "false",
        env = "BASE_NODE_CONDUCTOR_BINARY_COMMIT"
    )]
    pub conductor_binary_commit: bool,
}

impl Default for SequencerArgs {
    fn default() -> Self {
        // Construct default values using the clap parser.
        // This works since none of the cli flags are required.
        Self::parse_from::<[_; 0], &str>([])
    }
}

impl SequencerArgs {
    /// Creates a [`SequencerConfig`] from the [`SequencerArgs`].
    pub fn config(&self) -> SequencerConfig {
        SequencerConfig {
            sequencer_stopped: self.stopped,
            sequencer_recovery_mode: self.recover,
            conductor_rpc_url: self.conductor_rpc.clone(),
            conductor_binary_commit: self.conductor_binary_commit,
            conductor_rpc_timeout: self.conductor_rpc_timeout,
            l1_conf_delay: self.l1_confs,
        }
    }
}
