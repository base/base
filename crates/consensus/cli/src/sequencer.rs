//! Sequencer consensus-control CLI flags.

use std::{
    num::{NonZeroU64, ParseIntError},
    time::Duration,
};

use alloy_primitives::{
    Address, U256,
    utils::{Unit, parse_ether},
};
use base_consensus_node::{SequencerConfig, SequencerSyncMode, ShadowFunding};
use base_protocol::DEFAULT_SEAL_OFFSET;
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

    /// Request timeout for L1 RPC calls on the sequencer block-production hot path.
    #[arg(
        id = "sequencer_l1_rpc_timeout",
        long = "sequencer.l1-rpc-timeout-ms",
        default_value = SequencerConfig::DEFAULT_L1_RPC_TIMEOUT.as_millis().to_string(),
        env = "BASE_NODE_SEQUENCER_L1_RPC_TIMEOUT_MS",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        }
    )]
    pub l1_rpc_timeout: Duration,

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

    /// Account to fund in the first private block of every shadow sequencing cycle.
    #[arg(
        long = "sequencer.shadow-funding-address",
        env = "BASE_NODE_SEQUENCER_SHADOW_FUNDING_ADDRESS",
        requires = "shadow_blocks_per_cycle"
    )]
    pub shadow_funding_address: Option<Address>,

    /// ETH to mint into the shadow funding account. Defaults to 10,000 ETH when funding is enabled.
    #[arg(
        long = "sequencer.shadow-funding-amount-eth",
        env = "BASE_NODE_SEQUENCER_SHADOW_FUNDING_AMOUNT_ETH",
        requires = "shadow_funding_address",
        value_parser = parse_ether
    )]
    pub shadow_funding_amount: Option<U256>,

    /// Source used to complete the sequencer's initial sync.
    #[arg(
        id = "sequencer_sync_mode",
        long = "sequencer.sync-mode",
        default_value_t = SequencerSyncMode::default(),
        env = "BASE_NODE_SEQUENCER_SYNC_MODE"
    )]
    pub sync_mode: SequencerSyncMode,

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
            shadow_blocks_per_cycle: self.shadow_blocks_per_cycle,
            shadow_funding: self.shadow_funding_address.map(|address| {
                ShadowFunding::new(
                    address,
                    self.shadow_funding_amount
                        .unwrap_or_else(|| U256::from(10_000) * Unit::ETHER.wei()),
                )
            }),
            sequencer_sync_mode: self.sync_mode,
            conductor_rpc_url: self.conductor_rpc.clone(),
            conductor_binary_commit: self.conductor_binary_commit,
            conductor_rpc_timeout: self.conductor_rpc_timeout,
            l1_conf_delay: self.l1_confs,
            l1_rpc_timeout: self.l1_rpc_timeout,
            seal_offset: DEFAULT_SEAL_OFFSET,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU64, time::Duration};

    use alloy_primitives::{
        Address, U256, address,
        utils::{Unit, parse_ether},
    };
    use base_consensus_node::ShadowFunding;
    use clap::Parser;

    use super::{SequencerArgs, SequencerConfig, SequencerSyncMode};
    use crate::L1ClientArgs;

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        l1: L1ClientArgs,
        #[command(flatten)]
        sequencer: SequencerArgs,
    }

    #[test]
    fn defaults_l1_rpc_timeout_to_five_hundred_milliseconds() {
        let args = SequencerArgs::default();

        assert_eq!(args.l1_rpc_timeout, SequencerConfig::DEFAULT_L1_RPC_TIMEOUT);
        assert_eq!(args.config().l1_rpc_timeout, SequencerConfig::DEFAULT_L1_RPC_TIMEOUT);
    }

    #[test]
    fn sync_mode_parses_and_flows_to_config() {
        let args = SequencerArgs::parse_from(["base", "--sequencer.sync-mode", "el"]);

        assert_eq!(args.sync_mode, SequencerSyncMode::El);
        assert_eq!(args.config().sequencer_sync_mode, SequencerSyncMode::El);
    }

    #[test]
    fn parses_l1_rpc_timeout_in_milliseconds() {
        let args =
            SequencerArgs::parse_from(["base-consensus", "--sequencer.l1-rpc-timeout-ms", "750"]);

        assert_eq!(args.l1_rpc_timeout, Duration::from_millis(750));
        assert_eq!(args.config().l1_rpc_timeout, Duration::from_millis(750));
    }

    #[test]
    fn parses_general_and_sequencer_l1_rpc_timeouts_together() {
        let args = Command::parse_from([
            "base-consensus",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "--l1.rpc-timeout-ms",
            "2500",
            "--sequencer.l1-rpc-timeout-ms",
            "750",
        ]);

        assert_eq!(args.l1.l1_rpc_timeout, Duration::from_millis(2500));
        assert_eq!(args.sequencer.l1_rpc_timeout, Duration::from_millis(750));
    }

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
    fn defaults_shadow_funding_to_ten_thousand_eth() {
        let funding_address = address!("1111111111111111111111111111111111111111");
        let args = SequencerArgs::parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "12",
            "--sequencer.shadow-funding-address",
            "0x1111111111111111111111111111111111111111",
        ]);

        assert_eq!(
            args.config().shadow_funding,
            Some(ShadowFunding::new(funding_address, U256::from(10_000) * Unit::ETHER.wei()))
        );
    }

    #[test]
    fn parses_configured_shadow_funding_amount_in_eth() {
        let args = SequencerArgs::parse_from([
            "base-consensus",
            "--sequencer.shadow-blocks-per-cycle",
            "12",
            "--sequencer.shadow-funding-address",
            "0x1111111111111111111111111111111111111111",
            "--sequencer.shadow-funding-amount-eth",
            "12345",
        ]);

        let amount = parse_ether("12345").unwrap();
        assert_eq!(args.shadow_funding_amount, Some(amount));
        assert_eq!(
            args.config().shadow_funding,
            Some(ShadowFunding::new(Address::repeat_byte(0x11), amount))
        );
    }

    #[test]
    fn rejects_shadow_funding_without_shadow_mode() {
        let result = SequencerArgs::try_parse_from([
            "base-consensus",
            "--sequencer.shadow-funding-address",
            "0x1111111111111111111111111111111111111111",
        ]);

        assert!(result.is_err());
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
