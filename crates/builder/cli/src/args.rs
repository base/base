//! Builder CLI arguments and config conversion helpers.

use core::{net::SocketAddr, time::Duration};
use std::path::PathBuf;

use base_builder_core::{
    BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    ExecutionMeteringMode, RejectionCache, ShadowValidityConfig, SharedMeteringProvider,
};
use base_builder_metering::MeteringStore;
use base_execution_cli::ShadowIndexerArgs;
use base_node_core::{HasRollupArgs, RollupArgs};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY, TransactionEventProducer,
    TransactionEventWriterConfig,
};
use tracing::warn;

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

/// Dedicated transaction event journal configuration.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct TransactionEventsArgs {
    /// Enables dedicated transaction event JSONL writes.
    #[arg(
        long = "builder.transaction-events.enabled",
        env = "BUILDER_TRANSACTION_EVENTS_ENABLED",
        default_value = "false"
    )]
    pub enabled: bool,

    /// Dedicated transaction events JSONL file path.
    #[arg(
        long = "builder.transaction-events.file-path",
        env = "BUILDER_TRANSACTION_EVENTS_PATH",
        default_value = "/var/log/transaction-events/base-builder/events.jsonl"
    )]
    pub file_path: PathBuf,

    /// Bounded event queue capacity. Full queues drop events instead of blocking the builder.
    #[arg(long = "builder.transaction-events.queue-capacity", env = "BUILDER_TRANSACTION_EVENTS_QUEUE_CAPACITY", default_value_t = DEFAULT_QUEUE_CAPACITY)]
    pub queue_capacity: usize,

    /// Maximum size of an active transaction event JSONL segment.
    #[arg(long = "builder.transaction-events.max-file-bytes", env = "BUILDER_TRANSACTION_EVENTS_MAX_FILE_BYTES", default_value_t = DEFAULT_MAX_FILE_BYTES)]
    pub max_file_bytes: u64,

    /// Maximum number of transaction event JSONL segments to retain, including the active file.
    #[arg(long = "builder.transaction-events.max-files", env = "BUILDER_TRANSACTION_EVENTS_MAX_FILES", default_value_t = DEFAULT_MAX_FILES)]
    pub max_files: usize,

    /// Fail builder startup if the transaction event writer cannot open.
    #[arg(
        long = "builder.transaction-events.required",
        env = "BUILDER_TRANSACTION_EVENTS_REQUIRED",
        default_value = "false"
    )]
    pub required: bool,

    /// Network label to write into transaction event envelopes.
    #[arg(
        long = "builder.transaction-events.network",
        env = "BUILDER_TRANSACTION_EVENTS_NETWORK",
        default_value = "unknown"
    )]
    pub network: String,
}

impl Default for TransactionEventsArgs {
    fn default() -> Self {
        Self {
            enabled: false,
            file_path: PathBuf::from("/var/log/transaction-events/base-builder/events.jsonl"),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            required: false,
            network: "unknown".to_string(),
        }
    }
}

impl TransactionEventsArgs {
    /// Converts these args into the shared transaction event writer config.
    pub fn writer_config(&self) -> TransactionEventWriterConfig {
        TransactionEventWriterConfig {
            enabled: self.enabled,
            file_path: self.file_path.clone(),
            queue_capacity: self.queue_capacity,
            max_file_bytes: self.max_file_bytes,
            max_files: self.max_files,
            required: self.required,
            producer: TransactionEventProducer::BaseBuilder,
            network: self.network.clone(),
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

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    /// Scheduled for removal in v1.4.0 after rolling deployments have migrated.
    #[arg(long = "builder.flashblock-execution-time-budget-us", hide = true)]
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    /// Scheduled for removal in v1.4.0 after rolling deployments have migrated.
    #[arg(long = "builder.block-state-root-gas-limit", hide = true)]
    pub block_state_root_gas_limit: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    /// Scheduled for removal in v1.4.0 after rolling deployments have migrated.
    #[arg(long = "builder.state-root-gas-coefficient", hide = true)]
    pub state_root_gas_coefficient: Option<f64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    /// Scheduled for removal in v1.4.0 after rolling deployments have migrated.
    #[arg(long = "builder.state-root-gas-anchor-us", hide = true)]
    pub state_root_gas_anchor_us: Option<u128>,

    /// Execution metering mode: off, dry-run, or enforce
    #[arg(long = "builder.execution-metering-mode", value_enum, default_value = "off")]
    pub execution_metering_mode: ExecutionMeteringMode,

    /// How much extra time to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Whether to enable TIPS Resource Metering
    #[arg(long = "builder.enable-resource-metering", default_value = "false")]
    pub enable_resource_metering: bool,

    /// Enable experimental validity-bearing transactions on this builder.
    ///
    /// Registers `base_sendRawTransactionValidity` for direct ingress and accepts
    /// validity metadata on `base_insertValidatedTransaction` from forwarding nodes.
    /// Predicates are preserved and enforced during block construction.
    #[arg(long = "builder.enable-experimental-validity-transactions", default_value = "false")]
    pub enable_experimental_validity_transactions: bool,

    /// Maximum validity predicates accepted per experimental transaction.
    #[arg(
        long = "builder.experimental-validity-max-predicates",
        default_value_t = DEFAULT_MAX_VALIDITY_PREDICATES,
        requires = "enable_experimental_validity_transactions"
    )]
    pub experimental_validity_max_predicates: usize,

    /// Decorate sampled ordinary transactions with a behavior-preserving validity predicate.
    ///
    /// This must only be enabled on a shadow builder.
    #[arg(long = "builder.shadow-validity-injection.enabled", default_value = "false")]
    pub shadow_validity_injection_enabled: bool,

    /// Sampling rate for shadow validity injection, in basis points.
    #[arg(
        long = "builder.shadow-validity-injection.sample-rate-bps",
        default_value = "100",
        value_parser = clap::builder::RangedU64ValueParser::<u16>::new().range(1..=10_000)
    )]
    pub shadow_validity_injection_sample_rate_bps: u16,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes
    #[arg(long = "builder.max-uncompressed-block-size")]
    pub max_uncompressed_block_size: Option<u64>,

    /// Duration in milliseconds to wait for metering data before including a transaction.
    /// Transactions younger than this without metering data will be skipped.
    #[arg(long = "builder.metering-wait-duration-ms")]
    pub metering_wait_duration_ms: Option<u64>,

    /// Hard cutoff, in milliseconds, on cumulative validity-predicate evaluation time per
    /// builder iteration. Once exceeded, further validity-gated transactions are deferred to a
    /// later iteration rather than evaluated.
    #[arg(long = "builder.predicate-eval-hard-cutoff-ms", default_value = "10")]
    pub predicate_eval_hard_cutoff_ms: u64,

    /// URL of the audit-archiver RPC endpoint for forwarding rejected transactions
    #[arg(long = "builder.audit-archiver-url", env = "BUILDER_AUDIT_ARCHIVER_URL")]
    pub audit_archiver_url: Option<String>,

    /// Bounded channel capacity for rejected transaction forwarding (drops on full)
    #[arg(long = "builder.rejected-tx-channel-size", default_value = "500")]
    pub rejected_tx_channel_size: usize,

    /// Maximum rejected transactions accumulated per block before dropping
    #[arg(long = "builder.max-rejected-txs-per-block", default_value = "500")]
    pub max_rejected_txs_per_block: usize,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// TTL in seconds for entries in the metering store cache.
    /// Stale entries are evicted after this duration.
    #[arg(long = "builder.metering-store-ttl-secs", default_value = "30")]
    pub metering_store_ttl_secs: u64,

    /// Maximum number of entries in the rejection cache for permanently rejected transactions
    #[arg(long = "builder.rejection-cache-max-capacity", default_value = "100000")]
    pub rejection_cache_max_capacity: u64,

    /// TTL in seconds for entries in the rejection cache
    #[arg(long = "builder.rejection-cache-ttl-secs", default_value = "1800")]
    pub rejection_cache_ttl_secs: u64,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,

    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution. Disable with
    /// `--builder.eip8130-manifest-precheck=false`.
    #[arg(
        long = "builder.eip8130-manifest-precheck",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub manifest_precheck_enabled: bool,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,

    /// Deprecated compatibility flag; post-Beryl payload-builder cutover is always enabled.
    #[arg(long = "builder.payload-builder-cutover", default_value = "false", hide = true)]
    pub payload_builder_cutover: bool,

    /// Runs only the basic payload builder after the cutover is complete.
    #[arg(long = "builder.basic-payload-builder", default_value = "false")]
    pub basic_payload_builder: bool,

    /// Transaction event journal configuration
    #[command(flatten)]
    pub transaction_events: TransactionEventsArgs,

    /// Shadow indexer `ExEx` configuration
    #[command(flatten)]
    pub shadow_indexer: ShadowIndexerArgs,
}

impl HasRollupArgs for Args {
    fn rollup_args(&self) -> &RollupArgs {
        &self.rollup_args
    }
}

impl Args {
    /// Creates a [`MeteringStore`] from the CLI arguments.
    pub fn build_metering_store(&self) -> MeteringStore {
        MeteringStore::new(
            self.enable_resource_metering || self.execution_metering_mode.is_enabled(),
            self.tx_data_store_buffer_size,
            Duration::from_secs(self.metering_store_ttl_secs),
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
            state_root_gas_coefficient: None,
            state_root_gas_anchor_us: None,
            execution_metering_mode: ExecutionMeteringMode::Off,
            extra_block_deadline_secs: 20,
            enable_resource_metering: false,
            enable_experimental_validity_transactions: false,
            experimental_validity_max_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
            shadow_validity_injection_enabled: false,
            shadow_validity_injection_sample_rate_bps: 100,
            max_uncompressed_block_size: None,
            metering_wait_duration_ms: None,
            predicate_eval_hard_cutoff_ms: 10,
            audit_archiver_url: None,
            rejected_tx_channel_size: 500,
            max_rejected_txs_per_block: 500,
            tx_data_store_buffer_size: 10000,
            metering_store_ttl_secs: 30,
            rejection_cache_max_capacity: 100_000,
            rejection_cache_ttl_secs: 1800,
            sampling_ratio: 100,
            manifest_precheck_enabled: true,
            flashblocks: FlashblocksArgs::default(),
            payload_builder_cutover: false,
            basic_payload_builder: false,
            transaction_events: TransactionEventsArgs::default(),
            shadow_indexer: ShadowIndexerArgs::default(),
        }
    }
}

impl Args {
    /// Builds validated configuration for the builder transaction insertion RPC.
    ///
    /// # Errors
    ///
    /// Returns an error when shadow injection is enabled without validity transaction support.
    pub fn builder_api_config(&self) -> eyre::Result<BuilderApiExtensionConfig> {
        let shadow_validity = if self.shadow_validity_injection_enabled {
            ShadowValidityConfig::enabled(self.shadow_validity_injection_sample_rate_bps)?
        } else {
            ShadowValidityConfig::disabled()
        };
        Ok(BuilderApiExtensionConfig::new(
            self.enable_experimental_validity_transactions,
            self.experimental_validity_max_predicates,
        )
        .with_shadow_validity(shadow_validity)?)
    }

    /// Converts these CLI arguments into a [`BuilderConfig`] using the given shared metering
    /// provider. The same provider must also be passed to the RPC extension so that the
    /// building loop and the `base_setMeteringInformation` handler share a single store.
    pub fn into_builder_config(
        self,
        metering_provider: SharedMeteringProvider,
    ) -> eyre::Result<BuilderConfig> {
        if self.flashblock_execution_time_budget_us.is_some()
            || self.block_state_root_gas_limit.is_some()
            || self.state_root_gas_coefficient.is_some()
            || self.state_root_gas_anchor_us.is_some()
        {
            warn!("deprecated builder resource limit flags are ignored");
        }

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
            execution_metering_mode: self.execution_metering_mode,
            max_uncompressed_block_size: self.max_uncompressed_block_size,
            metering_wait_duration: self.metering_wait_duration_ms.map(Duration::from_millis),
            predicate_eval_hard_cutoff: Duration::from_millis(self.predicate_eval_hard_cutoff_ms),
            metering_provider,
            rejection_cache: RejectionCache::new(
                self.rejection_cache_max_capacity,
                Duration::from_secs(self.rejection_cache_ttl_secs),
            ),
            audit_archiver_url: self.audit_archiver_url,
            rejected_tx_channel_size: self.rejected_tx_channel_size,
            max_rejected_txs_per_block: self.max_rejected_txs_per_block,
            manifest_precheck_enabled: self.manifest_precheck_enabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{B256, TxHash, U256};
    use base_builder_core::{MeteringProvider, NoopMeteringProvider};
    use base_bundles::MeterBundleResponse;
    use clap::Parser;
    use rstest::rstest;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser {
        #[command(flatten)]
        args: Args,
    }

    fn convert(args: Args) -> BuilderConfig {
        let metering_provider: SharedMeteringProvider = Arc::new(NoopMeteringProvider);
        args.into_builder_config(metering_provider).expect("conversion should succeed")
    }

    #[test]
    fn builder_args_provides_embedded_rollup_args() {
        let args = Args::default();
        assert!(std::ptr::eq(args.rollup_args(), &args.rollup_args));
    }

    #[test]
    fn default_args_produce_valid_config() {
        let args = Args::default();
        assert!(!args.enable_experimental_validity_transactions);
        assert_eq!(args.experimental_validity_max_predicates, DEFAULT_MAX_VALIDITY_PREDICATES);
        assert!(!args.shadow_validity_injection_enabled);
        assert_eq!(args.shadow_validity_injection_sample_rate_bps, 100);
        assert!(!args.builder_api_config().unwrap().shadow_validity.is_enabled());
        let config = convert(args);
        assert_eq!(config.block_time, Duration::from_millis(1000));
        assert!(config.max_gas_per_txn.is_none());
        assert!(config.manifest_precheck_enabled);
    }

    #[test]
    fn experimental_validity_transactions_require_explicit_opt_in() {
        let parsed = CommandParser::parse_from([
            "builder",
            "--builder.enable-experimental-validity-transactions",
            "--builder.experimental-validity-max-predicates",
            "8",
        ]);

        assert!(parsed.args.enable_experimental_validity_transactions);
        assert_eq!(parsed.args.experimental_validity_max_predicates, 8);
    }

    #[test]
    fn shadow_validity_injection_requires_validity_support() {
        let args = Args { shadow_validity_injection_enabled: true, ..Default::default() };
        assert!(args.builder_api_config().is_err());

        let args = Args {
            enable_experimental_validity_transactions: true,
            shadow_validity_injection_enabled: true,
            shadow_validity_injection_sample_rate_bps: 250,
            ..Default::default()
        };
        let config = args.builder_api_config().unwrap();
        assert!(config.shadow_validity.is_enabled());
        assert_eq!(config.shadow_validity.sample_rate_basis_points(), 250);
    }

    #[test]
    fn shadow_validity_sampling_rate_is_cli_bounded() {
        let result = CommandParser::try_parse_from([
            "builder",
            "--builder.shadow-validity-injection.sample-rate-bps",
            "10001",
        ]);
        assert!(result.is_err());
    }

    #[rstest]
    #[case::enabled(true)]
    #[case::disabled(false)]
    fn manifest_precheck_flag_maps_to_config(#[case] enabled: bool) {
        let args = Args { manifest_precheck_enabled: enabled, ..Default::default() };
        assert_eq!(convert(args).manifest_precheck_enabled, enabled);
    }

    #[test]
    fn manifest_precheck_accepts_explicit_false() {
        let parsed =
            CommandParser::parse_from(["test", "--builder.eip8130-manifest-precheck=false"]);
        assert!(!parsed.args.manifest_precheck_enabled);
    }

    #[test]
    fn basic_payload_builder_defaults_to_disabled() {
        let parsed = CommandParser::parse_from(["test"]);
        assert!(!parsed.args.basic_payload_builder);
    }

    #[test]
    fn legacy_payload_builder_cutover_flag_is_accepted() {
        let parsed = CommandParser::parse_from(["test", "--builder.payload-builder-cutover"]);
        assert!(parsed.args.payload_builder_cutover);
    }

    #[test]
    fn basic_payload_builder_requires_explicit_opt_in() {
        let parsed = CommandParser::parse_from(["test", "--builder.basic-payload-builder"]);
        assert!(parsed.args.basic_payload_builder);
    }

    #[test]
    fn legacy_cutover_flag_is_compatible_with_basic_only() {
        let parsed = CommandParser::try_parse_from([
            "test",
            "--builder.payload-builder-cutover",
            "--builder.basic-payload-builder",
        ]);
        assert!(parsed.expect("legacy flag must not prevent startup").args.basic_payload_builder);
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
        let metering_provider: SharedMeteringProvider =
            Arc::new(MeteringStore::new(true, 100, Duration::from_secs(30)));
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
                total_gas_used: 21000,
                total_execution_time_us: 500,
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

    #[rstest]
    #[case::default(10, Duration::from_millis(10))]
    #[case::zero(0, Duration::from_millis(0))]
    #[case::custom(25, Duration::from_millis(25))]
    fn predicate_eval_hard_cutoff_maps_correctly(#[case] input: u64, #[case] expected: Duration) {
        let args = Args { predicate_eval_hard_cutoff_ms: input, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.predicate_eval_hard_cutoff, expected);
    }

    #[test]
    fn metering_store_ttl_propagates_to_store() {
        let args = Args {
            metering_store_ttl_secs: 60,
            enable_resource_metering: true,
            ..Default::default()
        };
        let store = args.build_metering_store();
        let tx_hash = TxHash::random();
        store.insert(
            tx_hash,
            MeterBundleResponse {
                bundle_hash: B256::ZERO,
                bundle_gas_price: U256::ZERO,
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                gas_fees: U256::ZERO,
                results: vec![],
                state_block_number: 0,
                total_gas_used: 21000,
                total_execution_time_us: 0,
            },
        );
        assert!(store.get(&tx_hash).is_some(), "entry should be present within TTL");
    }

    #[test]
    fn metering_store_ttl_defaults_to_30s() {
        let args = Args::default();
        assert_eq!(args.metering_store_ttl_secs, 30);
    }

    #[test]
    fn deprecated_resource_limit_flags_remain_accepted() {
        let args = CommandParser::parse_from([
            "builder",
            "--builder.flashblock-execution-time-budget-us",
            "5000000",
            "--builder.block-state-root-gas-limit",
            "1000000",
            "--builder.state-root-gas-coefficient",
            "0.1",
            "--builder.state-root-gas-anchor-us",
            "5000",
        ])
        .args;

        assert_eq!(args.flashblock_execution_time_budget_us, Some(5_000_000));
        assert_eq!(args.block_state_root_gas_limit, Some(1_000_000));
        assert_eq!(args.state_root_gas_coefficient, Some(0.1));
        assert_eq!(args.state_root_gas_anchor_us, Some(5_000));
    }

    #[test]
    fn combined_overrides_work_together() {
        let args = Args {
            chain_block_time: 2000,
            max_gas_per_txn: Some(100000),
            max_execution_time_per_tx_us: Some(5000),
            execution_metering_mode: ExecutionMeteringMode::Enforce,
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
        assert_eq!(config.max_execution_time_per_tx_us, Some(5000));
        assert_eq!(config.execution_metering_mode, ExecutionMeteringMode::Enforce);
        assert_eq!(config.block_time_leeway, Duration::from_secs(10));
        assert_eq!(config.flashblocks_interval, Duration::from_millis(200));
        assert_eq!(config.flashblocks_leeway_time, Duration::from_millis(50));
    }
}
