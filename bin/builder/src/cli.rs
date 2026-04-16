//! Contains the CLI arguments

use core::{net::SocketAddr, time::Duration};

use base_builder_core::{
    BuilderConfig, ExecutionMeteringMode, RejectionCache, SharedMeteringProvider,
};
use base_builder_metering::MeteringStore;
use base_node_core::args::RollupArgs;

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

    /// Time by which blocks would be completed earlier in milliseconds.
    ///
    /// This time is used to account for latencies and would be deducted from total block
    /// building time before calculating number of fbs.
    #[arg(long = "flashblocks.leeway-time", default_value = "75", env = "FLASHBLOCK_LEEWAY_TIME")]
    pub flashblocks_leeway_time: u64,
}

impl Default for FlashblocksArgs {
    fn default() -> Self {
        Self {
            flashblocks_port: 1111,
            flashblocks_addr: "127.0.0.1".to_string(),
            flashblocks_block_time: 250,
            flashblocks_leeway_time: 75,
        }
    }
}

/// Parameters for rollup configuration
#[derive(Debug, Clone, clap::Args)]
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

    /// Flashblock-level execution time budget in microseconds (requires resource metering)
    #[arg(long = "builder.flashblock-execution-time-budget-us")]
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root gas limit (requires resource metering)
    #[arg(long = "builder.block-state-root-gas-limit")]
    pub block_state_root_gas_limit: Option<u64>,

    /// State root gas coefficient (K): controls how excess SR time inflates `sr_gas` cost
    #[arg(long = "builder.state-root-gas-coefficient", default_value = "0.02")]
    pub state_root_gas_coefficient: f64,

    /// State root gas anchor in microseconds: SR below this produces no penalty
    #[arg(long = "builder.state-root-gas-anchor-us", default_value = "5000")]
    pub state_root_gas_anchor_us: u128,

    /// Execution metering mode: off, dry-run, or enforce
    #[arg(long = "builder.execution-metering-mode", value_enum, default_value = "off")]
    pub execution_metering_mode: ExecutionMeteringMode,

    /// How much extra time to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Whether to enable TIPS Resource Metering
    #[arg(long = "builder.enable-resource-metering", default_value = "false")]
    pub enable_resource_metering: bool,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes
    #[arg(long = "builder.max-uncompressed-block-size")]
    pub max_uncompressed_block_size: Option<u64>,

    /// Duration in milliseconds to wait for metering data before including a transaction.
    /// Transactions younger than this without metering data will be skipped.
    #[arg(long = "builder.metering-wait-duration-ms")]
    pub metering_wait_duration_ms: Option<u64>,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// Maximum number of entries in the rejection cache for permanently rejected transactions
    #[arg(long = "builder.rejection-cache-max-capacity", default_value = "100000")]
    pub rejection_cache_max_capacity: u64,

    /// TTL in seconds for entries in the rejection cache
    #[arg(long = "builder.rejection-cache-ttl-secs", default_value = "1800")]
    pub rejection_cache_ttl_secs: u64,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
}

impl Args {
    /// Creates a [`MeteringStore`] from the CLI arguments.
    pub fn build_metering_store(&self) -> MeteringStore {
        MeteringStore::new(
            self.enable_resource_metering || self.execution_metering_mode.is_enabled(),
            self.tx_data_store_buffer_size,
        )
    }
}

impl Default for Args {
    fn default() -> Self {
        Self {
            rollup_args: RollupArgs::default(),
            chain_block_time: 1000,
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_gas_limit: None,
            state_root_gas_coefficient: 0.02,
            state_root_gas_anchor_us: 5000,
            execution_metering_mode: ExecutionMeteringMode::Off,
            extra_block_deadline_secs: 20,
            enable_resource_metering: false,
            max_uncompressed_block_size: None,
            metering_wait_duration_ms: None,
            tx_data_store_buffer_size: 10000,
            rejection_cache_max_capacity: 100_000,
            rejection_cache_ttl_secs: 1800,
            sampling_ratio: 100,
            flashblocks: FlashblocksArgs::default(),
        }
    }
}

impl Args {
    /// Converts these CLI arguments into a [`BuilderConfig`] using the given shared metering
    /// provider. The same provider must also be passed to the RPC extension so that the
    /// building loop and the `base_setMeteringInformation` handler share a single store.
    pub fn into_builder_config(
        self,
        metering_provider: SharedMeteringProvider,
    ) -> eyre::Result<BuilderConfig> {
        let flashblocks_ws_addr = SocketAddr::new(
            self.flashblocks.flashblocks_addr.parse()?,
            self.flashblocks.flashblocks_port,
        );

        Ok(BuilderConfig {
            block_time: Duration::from_millis(self.chain_block_time),
            block_time_leeway: Duration::from_secs(self.extra_block_deadline_secs),
            da_config: Default::default(),
            gas_limit_config: Default::default(),
            sampling_ratio: self.sampling_ratio,
            flashblocks_ws_addr,
            flashblocks_interval: Duration::from_millis(self.flashblocks.flashblocks_block_time),
            flashblocks_leeway_time: Duration::from_millis(
                self.flashblocks.flashblocks_leeway_time,
            ),
            max_gas_per_txn: self.max_gas_per_txn,
            max_execution_time_per_tx_us: self.max_execution_time_per_tx_us,
            flashblock_execution_time_budget_us: self.flashblock_execution_time_budget_us,
            block_state_root_gas_limit: self.block_state_root_gas_limit,
            state_root_gas_coefficient: self.state_root_gas_coefficient,
            state_root_gas_anchor_us: self.state_root_gas_anchor_us,
            execution_metering_mode: self.execution_metering_mode,
            max_uncompressed_block_size: self.max_uncompressed_block_size,
            metering_wait_duration: self.metering_wait_duration_ms.map(Duration::from_millis),
            metering_provider,
            rejection_cache: RejectionCache::new(
                self.rejection_cache_max_capacity,
                Duration::from_secs(self.rejection_cache_ttl_secs),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rstest::rstest;

    use super::*;

    fn convert(args: Args) -> BuilderConfig {
        let metering_provider: SharedMeteringProvider =
            Arc::new(base_builder_core::NoopMeteringProvider);
        args.into_builder_config(metering_provider).expect("conversion should succeed")
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
        assert_eq!(config.flashblocks_interval, Duration::from_millis(expected_ms));
    }

    #[test]
    fn metering_data_written_to_provider_is_readable_from_config() {
        use alloy_primitives::{B256, TxHash, U256};
        use base_bundles::MeterBundleResponse;

        let metering_provider: SharedMeteringProvider = Arc::new(MeteringStore::new(true, 100));
        let args = Args { enable_resource_metering: true, ..Default::default() };
        let config = args
            .into_builder_config(Arc::clone(&metering_provider))
            .expect("conversion should succeed");

        let tx_hash = TxHash::random();
        metering_provider.insert(
            tx_hash,
            MeterBundleResponse {
                bundle_hash: B256::ZERO,
                bundle_gas_price: U256::ZERO,
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                gas_fees: U256::ZERO,
                results: vec![],
                state_block_number: 0,
                state_flashblock_index: None,
                total_gas_used: 21000,
                total_execution_time_us: 500,
                state_root_time_us: 100,
                state_root_account_leaf_count: 0,
                state_root_account_branch_count: 0,
                state_root_storage_leaf_count: 0,
                state_root_storage_branch_count: 0,
            },
        );

        let result = config.metering_provider.get(&tx_hash);
        assert_eq!(result.unwrap().total_execution_time_us, 500);
    }

    #[rstest]
    #[case::some_duration(Some(500), Some(Duration::from_millis(500)))]
    #[case::none(None, None)]
    #[case::zero(Some(0), Some(Duration::from_millis(0)))]
    fn metering_wait_duration_maps_correctly(
        #[case] input: Option<u64>,
        #[case] expected: Option<Duration>,
    ) {
        let args = Args { metering_wait_duration_ms: input, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.metering_wait_duration, expected);
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
                ..Default::default()
            },
            ..Default::default()
        };
        let config = convert(args);

        assert_eq!(config.block_time, Duration::from_millis(2000));
        assert_eq!(config.max_gas_per_txn, Some(100000));
        assert_eq!(config.block_time_leeway, Duration::from_secs(10));
        assert_eq!(config.flashblocks_interval, Duration::from_millis(200));
        assert_eq!(config.flashblocks_leeway_time, Duration::from_millis(50));
    }
}
