//! Additional Node command arguments for the OP builder.
//!
//! Copied from `OptimismNode` to allow easy extension.

use core::{convert::TryFrom, net::SocketAddr, time::Duration};

use base_builder_core::{BuilderConfig, FlashblocksConfig, TxDataStore};
use reth_optimism_node::args::RollupArgs;

use crate::{FlashblocksArgs, TelemetryArgs};

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct BuilderArgs {
    /// Rollup configuration
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// chain block time in milliseconds
    #[arg(long = "rollup.chain-block-time", default_value = "1000", env = "CHAIN_BLOCK_TIME")]
    pub chain_block_time: u64,

    /// max gas a transaction can use
    #[arg(long = "builder.max_gas_per_txn")]
    pub max_gas_per_txn: Option<u64>,

    /// How much extra time to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Whether to enable TIPS Resource Metering
    #[arg(long = "builder.enable-resource-metering", default_value = "false")]
    pub enable_resource_metering: bool,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
    /// Telemetry configuration
    #[command(flatten)]
    pub telemetry: TelemetryArgs,
}

impl Default for BuilderArgs {
    fn default() -> Self {
        Self {
            rollup_args: RollupArgs::default(),
            chain_block_time: 1000,
            max_gas_per_txn: None,
            extra_block_deadline_secs: 20,
            enable_resource_metering: false,
            tx_data_store_buffer_size: 10000,
            flashblocks: FlashblocksArgs::default(),
            telemetry: TelemetryArgs::default(),
        }
    }
}

impl TryFrom<BuilderArgs> for BuilderConfig {
    type Error = eyre::Report;

    fn try_from(args: BuilderArgs) -> Result<Self, Self::Error> {
        let flashblocks = FlashblocksConfig::try_from(args.clone())?;
        Ok(Self {
            block_time: Duration::from_millis(args.chain_block_time),
            block_time_leeway: Duration::from_secs(args.extra_block_deadline_secs),
            da_config: Default::default(),
            gas_limit_config: Default::default(),
            sampling_ratio: args.telemetry.sampling_ratio,
            max_gas_per_txn: args.max_gas_per_txn,
            tx_data_store: TxDataStore::new(
                args.enable_resource_metering,
                args.tx_data_store_buffer_size,
            ),
            flashblocks,
        })
    }
}

impl TryFrom<BuilderArgs> for FlashblocksConfig {
    type Error = eyre::Report;

    fn try_from(args: BuilderArgs) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.flashblocks.flashblocks_block_time);

        let ws_addr = SocketAddr::new(
            args.flashblocks.flashblocks_addr.parse()?,
            args.flashblocks.flashblocks_port,
        );

        let leeway_time = Duration::from_millis(args.flashblocks.flashblocks_leeway_time);

        let fixed = args.flashblocks.flashblocks_fixed;

        let disable_state_root = args.flashblocks.flashblocks_disable_state_root;

        let compute_state_root_on_finalize =
            args.flashblocks.flashblocks_compute_state_root_on_finalize;

        Ok(Self {
            ws_addr,
            interval,
            leeway_time,
            fixed,
            disable_state_root,
            compute_state_root_on_finalize,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::FlashblocksArgs;

    fn convert(args: BuilderArgs) -> BuilderConfig {
        BuilderConfig::try_from(args).expect("conversion should succeed")
    }

    #[test]
    fn default_args_produce_valid_config() {
        let config = convert(BuilderArgs::default());
        assert_eq!(config.block_time, Duration::from_millis(1000));
        assert!(config.max_gas_per_txn.is_none());
    }

    #[rstest]
    #[case::block_time_1s(1000, 1000)]
    #[case::block_time_2s(2000, 2000)]
    #[case::block_time_250ms(250, 250)]
    fn chain_block_time_maps_to_block_time(#[case] input_ms: u64, #[case] expected_ms: u64) {
        let args = BuilderArgs { chain_block_time: input_ms, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.block_time, Duration::from_millis(expected_ms));
    }

    #[rstest]
    #[case::some_gas(Some(50000), Some(50000))]
    #[case::none(None, None)]
    #[case::large_gas(Some(1_000_000), Some(1_000_000))]
    fn max_gas_per_txn_maps_correctly(#[case] input: Option<u64>, #[case] expected: Option<u64>) {
        let args = BuilderArgs { max_gas_per_txn: input, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.max_gas_per_txn, expected);
    }

    #[rstest]
    #[case::leeway_30s(30, 30)]
    #[case::leeway_10s(10, 10)]
    #[case::leeway_0s(0, 0)]
    fn extra_block_deadline_maps_to_leeway(#[case] input_secs: u64, #[case] expected_secs: u64) {
        let args = BuilderArgs { extra_block_deadline_secs: input_secs, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.block_time_leeway, Duration::from_secs(expected_secs));
    }

    #[rstest]
    #[case::interval_500ms(500, 500)]
    #[case::interval_200ms(200, 200)]
    #[case::interval_250ms(250, 250)]
    fn flashblocks_interval_maps_correctly(#[case] input_ms: u64, #[case] expected_ms: u64) {
        let args = BuilderArgs {
            flashblocks: FlashblocksArgs { flashblocks_block_time: input_ms, ..Default::default() },
            ..Default::default()
        };
        let config = convert(args);
        assert_eq!(config.flashblocks.interval, Duration::from_millis(expected_ms));
    }

    #[rstest]
    #[case::fixed_true(true, true)]
    #[case::fixed_false(false, false)]
    fn flashblocks_fixed_mode_maps_correctly(#[case] input: bool, #[case] expected: bool) {
        let args = BuilderArgs {
            flashblocks: FlashblocksArgs { flashblocks_fixed: input, ..Default::default() },
            ..Default::default()
        };
        let config = convert(args);
        assert_eq!(config.flashblocks.fixed, expected);
    }

    #[rstest]
    #[case::both_enabled(true, true, true, true)]
    #[case::both_disabled(false, false, false, false)]
    #[case::disable_only(true, false, true, false)]
    #[case::finalize_only(false, true, false, true)]
    fn flashblocks_state_root_options_map_correctly(
        #[case] disable_input: bool,
        #[case] finalize_input: bool,
        #[case] disable_expected: bool,
        #[case] finalize_expected: bool,
    ) {
        let args = BuilderArgs {
            flashblocks: FlashblocksArgs {
                flashblocks_disable_state_root: disable_input,
                flashblocks_compute_state_root_on_finalize: finalize_input,
                ..Default::default()
            },
            ..Default::default()
        };
        let config = convert(args);
        assert_eq!(config.flashblocks.disable_state_root, disable_expected);
        assert_eq!(config.flashblocks.compute_state_root_on_finalize, finalize_expected);
    }

    #[test]
    fn combined_overrides_work_together() {
        let args = BuilderArgs {
            chain_block_time: 2000,
            max_gas_per_txn: Some(100000),
            extra_block_deadline_secs: 10,
            flashblocks: FlashblocksArgs {
                flashblocks_block_time: 200,
                flashblocks_leeway_time: 50,
                flashblocks_fixed: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let config = convert(args);

        assert_eq!(config.block_time, Duration::from_millis(2000));
        assert_eq!(config.max_gas_per_txn, Some(100000));
        assert_eq!(config.block_time_leeway, Duration::from_secs(10));
        assert_eq!(config.flashblocks.interval, Duration::from_millis(200));
        assert_eq!(config.flashblocks.leeway_time, Duration::from_millis(50));
        assert!(config.flashblocks.fixed);
    }
}
