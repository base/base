//! Contains the CLI arguments

use std::sync::Arc;

use base_reth_runner::{
    BaseNodeConfig, FlashblocksCell, FlashblocksConfig, KafkaConfig, MeteringConfig,
    ResourceLimitsConfig, TracingConfig,
};
use once_cell::sync::OnceCell;
use reth_optimism_node::args::RollupArgs;
use reth_optimism_payload_builder::config::OpDAConfig;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// The websocket url used for flashblocks.
    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    /// The max pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    // --- Priority fee estimation args ---
    /// Path to Kafka properties file (required for priority fee estimation).
    /// The properties file should contain rdkafka settings like bootstrap.servers,
    /// group.id, session.timeout.ms, etc.
    #[arg(long = "metering-kafka-properties-file")]
    pub metering_kafka_properties_file: Option<String>,

    /// Kafka topic for accepted bundle events
    #[arg(long = "metering-kafka-topic", default_value = "tips-ingress")]
    pub metering_kafka_topic: String,

    /// Kafka consumer group ID (overrides group.id in properties file if set)
    #[arg(long = "metering-kafka-group-id")]
    pub metering_kafka_group_id: Option<String>,

    /// Gas limit per flashblock for priority fee estimation
    #[arg(long = "metering-gas-limit", default_value = "30000000")]
    pub metering_gas_limit: u64,

    /// Execution time budget in microseconds per flashblock
    #[arg(long = "metering-execution-time-us", default_value = "50000")]
    pub metering_execution_time_us: u64,

    /// State root time budget in microseconds (optional, disabled by default)
    #[arg(long = "metering-state-root-time-us")]
    pub metering_state_root_time_us: Option<u64>,

    /// Data availability bytes limit per flashblock (default).
    /// This value is used when `miner_setMaxDASize` has not been called.
    #[arg(long = "metering-da-bytes", default_value = "120000")]
    pub metering_da_bytes: u64,

    /// Percentile for recommended priority fee (0.0-1.0)
    #[arg(long = "metering-priority-fee-percentile", default_value = "0.5")]
    pub metering_priority_fee_percentile: f64,

    /// Default priority fee when resource is not congested (in wei)
    #[arg(long = "metering-uncongested-priority-fee", default_value = "1")]
    pub metering_uncongested_priority_fee: u128,

    /// Number of recent blocks to retain in metering cache
    #[arg(long = "metering-cache-size", default_value = "12")]
    pub metering_cache_size: usize,
}

impl Args {
    /// Returns if flashblocks is enabled.
    /// If the websocket url is specified through the CLI.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }
}

impl From<Args> for BaseNodeConfig {
    fn from(args: Args) -> Self {
        let flashblocks_cell: FlashblocksCell = Arc::new(OnceCell::new());
        let flashblocks = args.websocket_url.map(|websocket_url| FlashblocksConfig {
            websocket_url,
            max_pending_blocks_depth: args.max_pending_blocks_depth,
        });

        // Build Kafka config if properties file is provided
        let kafka = args.metering_kafka_properties_file.map(|properties_file| KafkaConfig {
            properties_file,
            topic: args.metering_kafka_topic,
            group_id_override: args.metering_kafka_group_id,
        });

        let metering = MeteringConfig {
            enabled: args.enable_metering,
            kafka,
            resource_limits: ResourceLimitsConfig {
                gas_limit: args.metering_gas_limit,
                execution_time_us: args.metering_execution_time_us,
                state_root_time_us: args.metering_state_root_time_us,
                da_bytes: args.metering_da_bytes,
            },
            priority_fee_percentile: args.metering_priority_fee_percentile,
            uncongested_priority_fee: args.metering_uncongested_priority_fee,
            cache_size: args.metering_cache_size,
        };

        // Create shared DA config. This is shared between the payload builder and the
        // priority fee estimator, allowing miner_setMaxDASize to affect both.
        let da_config = OpDAConfig::default();

        Self {
            rollup_args: args.rollup_args,
            flashblocks,
            tracing: TracingConfig {
                enabled: args.enable_transaction_tracing,
                logs_enabled: args.enable_transaction_tracing_logs,
            },
            metering_enabled: args.enable_metering,
            metering,
            flashblocks_cell,
            da_config,
        }
    }
}
