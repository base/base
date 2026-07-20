//! Sequencer consensus-control CLI flags.

use std::{num::NonZeroU64, time::Duration};

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

    /// Number of private blocks to build before reconciling to canonical P2P payloads.
    ///
    /// Providing this value enables shadow sequencer mode.
    #[arg(
        long = "sequencer.shadow-blocks-per-cycle",
        env = "BASE_NODE_SEQUENCER_SHADOW_BLOCKS_PER_CYCLE",
        value_parser = clap::builder::RangedU64ValueParser::<NonZeroU64>::new()
            .range(1..=SequencerConfig::MAX_SHADOW_BLOCKS_PER_CYCLE)
    )]
    pub shadow_blocks_per_cycle: Option<NonZeroU64>,

    /// Conductor service RPC endpoint. Providing this value enables the conductor service.
    #[arg(long = "conductor.rpc", env = "BASE_NODE_CONDUCTOR_RPC")]
    pub conductor_rpc: Option<Url>,

    /// Conductor service RPC timeout.
    #[arg(
        long = "conductor.rpc.timeout",
        default_value = "1s",
        env = "BASE_NODE_CONDUCTOR_RPC_TIMEOUT",
        value_parser = parse_duration
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

fn parse_duration(arg: &str) -> Result<Duration, String> {
    humantime::parse_duration(arg).map_err(|error| error.to_string())
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
            shadow_blocks_per_cycle: self.shadow_blocks_per_cycle,
            conductor_rpc_url: self.conductor_rpc.clone(),
            conductor_binary_commit: self.conductor_binary_commit,
            conductor_rpc_timeout: self.conductor_rpc_timeout,
            l1_conf_delay: self.l1_confs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use clap::Parser;

    use super::SequencerArgs;

    #[test]
    fn parses_shadow_blocks_per_cycle() {
        let args = SequencerArgs::parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "12",
        ]);

        assert_eq!(args.shadow_blocks_per_cycle, NonZeroU64::new(12));
        assert_eq!(args.config().shadow_blocks_per_cycle, NonZeroU64::new(12));
    }

    #[test]
    fn rejects_zero_shadow_blocks_per_cycle() {
        let result = SequencerArgs::try_parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "0",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn accepts_maximum_shadow_blocks_per_cycle() {
        let result = SequencerArgs::try_parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "300",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_too_many_shadow_blocks_per_cycle() {
        let result = SequencerArgs::try_parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "301",
        ]);
        assert!(result.is_err());
    }
}
