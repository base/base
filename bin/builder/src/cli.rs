//! Contains the CLI arguments

use core::{convert::TryFrom, net::SocketAddr, time::Duration};

use base_builder_core::{BuilderConfig, FlashblocksConfig, ResourceMeteringMode, TxDataStore};
use reth_optimism_node::args::RollupArgs;

/// Parameters for Flashblocks configuration.
///
/// The names in the struct are prefixed with `flashblocks` to avoid conflicts
/// with the legacy standard builder configuration (now removed) since these args are
/// flattened into the main `Args` struct with the other rollup/node args.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct FlashblocksArgs {
    /// Flashblocks is always enabled; these options tune its behavior.
    /// The port that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.port", env = "FLASHBLOCKS_WS_PORT", default_value = "1111")]
    pub flashblocks_port: u16,

    /// The address that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.addr", env = "FLASHBLOCKS_WS_ADDR", default_value = "127.0.0.1")]
    pub flashblocks_addr: String,

    /// flashblock block time in milliseconds
    #[arg(long = "flashblocks.block-time", default_value = "250", env = "FLASHBLOCK_BLOCK_TIME")]
    pub flashblocks_block_time: u64,

    /// Builder would always try to produce fixed number of flashblocks without regard to time of
    /// FCU arrival.
    /// In cases of late FCU it could lead to partially filled blocks.
    #[arg(long = "flashblocks.fixed", default_value = "false", env = "FLASHBLOCK_FIXED")]
    pub flashblocks_fixed: bool,

    /// Time by which blocks would be completed earlier in milliseconds.
    ///
    /// This time is used to account for latencies and would be deducted from total block
    /// building time before calculating number of fbs.
    #[arg(long = "flashblocks.leeway-time", default_value = "75", env = "FLASHBLOCK_LEEWAY_TIME")]
    pub flashblocks_leeway_time: u64,

    /// Whether to disable state root calculation for each flashblock
    #[arg(
        long = "flashblocks.disable-state-root",
        default_value = "false",
        env = "FLASHBLOCKS_DISABLE_STATE_ROOT"
    )]
    pub flashblocks_disable_state_root: bool,

    /// Whether to compute state root only when `get_payload` is called (finalization).
    /// When enabled, flashblocks are built without state root, but the final payload
    /// returned by `get_payload` will have the state root computed.
    /// Requires --flashblocks.disable-state-root to be effective.
    #[arg(
        long = "flashblocks.compute-state-root-on-finalize",
        default_value = "false",
        env = "FLASHBLOCKS_COMPUTE_STATE_ROOT_ON_FINALIZE"
    )]
    pub flashblocks_compute_state_root_on_finalize: bool,
}

impl Default for FlashblocksArgs {
    fn default() -> Self {
        Self {
            flashblocks_port: 1111,
            flashblocks_addr: "127.0.0.1".to_string(),
            flashblocks_block_time: 250,
            flashblocks_fixed: false,
            flashblocks_leeway_time: 75,
            flashblocks_disable_state_root: false,
            flashblocks_compute_state_root_on_finalize: false,
        }
    }
}

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup configuration
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// chain block time in milliseconds
    #[arg(long = "rollup.chain-block-time", default_value = "1000", env = "CHAIN_BLOCK_TIME")]
    pub chain_block_time: u64,

    /// max gas a transaction can use
    #[arg(long = "builder.max_gas_per_txn")]
    pub max_gas_per_txn: Option<u64>,

    /// Maximum execution time per transaction in microseconds (requires resource metering)
    #[arg(long = "builder.max-execution-time-per-tx-us")]
    pub max_execution_time_per_tx_us: Option<u128>,

    /// Maximum state root calculation time per transaction in microseconds (requires resource metering)
    #[arg(long = "builder.max-state-root-time-per-tx-us")]
    pub max_state_root_time_per_tx_us: Option<u128>,

    /// Flashblock-level execution time budget in microseconds (requires resource metering)
    #[arg(long = "builder.flashblock-execution-time-budget-us")]
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root calculation time budget in microseconds (requires resource metering)
    #[arg(long = "builder.block-state-root-time-budget-us")]
    pub block_state_root_time_budget_us: Option<u128>,

    /// Resource metering mode: off, dry-run, or enforce
    #[arg(long = "builder.resource-metering-mode", value_enum, default_value = "off")]
    pub resource_metering_mode: ResourceMeteringMode,

    /// How much extra time to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Whether to enable TIPS Resource Metering
    #[arg(long = "builder.enable-resource-metering", default_value = "false")]
    pub enable_resource_metering: bool,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            rollup_args: RollupArgs::default(),
            chain_block_time: 1000,
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            resource_metering_mode: ResourceMeteringMode::Off,
            extra_block_deadline_secs: 20,
            enable_resource_metering: false,
            tx_data_store_buffer_size: 10000,
            sampling_ratio: 100,
            flashblocks: FlashblocksArgs::default(),
        }
    }
}

impl TryFrom<Args> for BuilderConfig {
    type Error = eyre::Report;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        let flashblocks = FlashblocksConfig::try_from(&args)?;
        Ok(Self {
            block_time: Duration::from_millis(args.chain_block_time),
            block_time_leeway: Duration::from_secs(args.extra_block_deadline_secs),
            da_config: Default::default(),
            gas_limit_config: Default::default(),
            sampling_ratio: args.sampling_ratio,
            max_gas_per_txn: args.max_gas_per_txn,
            max_execution_time_per_tx_us: args.max_execution_time_per_tx_us,
            max_state_root_time_per_tx_us: args.max_state_root_time_per_tx_us,
            flashblock_execution_time_budget_us: args.flashblock_execution_time_budget_us,
            block_state_root_time_budget_us: args.block_state_root_time_budget_us,
            resource_metering_mode: args.resource_metering_mode,
            tx_data_store: TxDataStore::new(
                args.enable_resource_metering || args.resource_metering_mode.is_enabled(),
                args.tx_data_store_buffer_size,
            ),
            flashblocks,
        })
    }
}

impl TryFrom<&Args> for FlashblocksConfig {
    type Error = eyre::Report;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.flashblocks.flashblocks_block_time);

        let ws_addr = SocketAddr::new(
            args.flashblocks.flashblocks_addr.parse()?,
            args.flashblocks.flashblocks_port,
        );

        let leeway_time = Duration::from_millis(args.flashblocks.flashblocks_leeway_time);

        Ok(Self {
            ws_addr,
            interval,
            leeway_time,
            fixed: args.flashblocks.flashblocks_fixed,
            disable_state_root: args.flashblocks.flashblocks_disable_state_root,
            compute_state_root_on_finalize: args
                .flashblocks
                .flashblocks_compute_state_root_on_finalize,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn convert(args: Args) -> BuilderConfig {
        BuilderConfig::try_from(args).expect("conversion should succeed")
    }

    #[test]
    fn default_args_produce_valid_config() {
        let config = convert(Args::default());
        assert_eq!(config.block_time, Duration::from_millis(1000));
        assert!(config.max_gas_per_txn.is_none());
    }

    #[rstest]
    #[case::block_time_1s(1000, 1000)]
    #[case::block_time_2s(2000, 2000)]
    #[case::block_time_250ms(250, 250)]
    fn chain_block_time_maps_to_block_time(#[case] input_ms: u64, #[case] expected_ms: u64) {
        let args = Args { chain_block_time: input_ms, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.block_time, Duration::from_millis(expected_ms));
    }

    #[rstest]
    #[case::some_gas(Some(50000), Some(50000))]
    #[case::none(None, None)]
    #[case::large_gas(Some(1_000_000), Some(1_000_000))]
    fn max_gas_per_txn_maps_correctly(#[case] input: Option<u64>, #[case] expected: Option<u64>) {
        let args = Args { max_gas_per_txn: input, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.max_gas_per_txn, expected);
    }

    #[rstest]
    #[case::leeway_30s(30, 30)]
    #[case::leeway_10s(10, 10)]
    #[case::leeway_0s(0, 0)]
    fn extra_block_deadline_maps_to_leeway(#[case] input_secs: u64, #[case] expected_secs: u64) {
        let args = Args { extra_block_deadline_secs: input_secs, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.block_time_leeway, Duration::from_secs(expected_secs));
    }

    #[rstest]
    #[case::interval_500ms(500, 500)]
    #[case::interval_200ms(200, 200)]
    #[case::interval_250ms(250, 250)]
    fn flashblocks_interval_maps_correctly(#[case] input_ms: u64, #[case] expected_ms: u64) {
        let args = Args {
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
        let args = Args {
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
        let args = Args {
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
        let args = Args {
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
