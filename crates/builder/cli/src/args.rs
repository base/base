//! Builder CLI arguments and config conversion helpers.

use core::time::Duration;
use std::{path::PathBuf, sync::Arc};

use base_builder_core::{
    BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES, RejectionCache,
    ResourceMeteringConfig, RestingPredicateMode, SharedMeteringProvider, ValiditySignatureMode,
};
use base_builder_metering::MeteringStore;
use base_execution_cli::ShadowIndexerArgs;
use base_node_core::{HasRollupArgs, RollupArgs};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY, TransactionEventProducer,
    TransactionEventWriterConfig,
};
use eyre::WrapErr;
use tracing::{info, warn};

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

    /// Enables builder resource metering: the shared metering store (`MeteringStore`) and
    /// native payload resource-metering schedule evaluation
    /// (`--payload.resource-metering-schedule`).
    ///
    /// This is the single on/off switch for builder metering: when disabled, neither runs.
    #[arg(
        long = "builder.enable-resource-metering",
        env = "BUILDER_ENABLE_RESOURCE_METERING",
        default_value = "false"
    )]
    pub enable_resource_metering: bool,

    /// JSON file containing the startup resource-metering schedule.
    ///
    /// Evaluated by the native payload builder when `--builder.enable-resource-metering`
    /// (`BUILDER_ENABLE_RESOURCE_METERING`) is enabled. Each dimension in the
    /// schedule must explicitly set `dryRun` (`true` to observe budgets without
    /// excluding transactions, `false` to enforce).
    ///
    /// Uses the same `--payload.resource-metering-schedule` flag and
    /// `PAYLOAD_RESOURCE_METERING_SCHEDULE` environment variable as the
    /// standalone execution node so operators can share one schedule path.
    #[arg(long = "payload.resource-metering-schedule", env = "PAYLOAD_RESOURCE_METERING_SCHEDULE")]
    pub resource_metering_schedule: Option<PathBuf>,

    /// Maximum validity predicates accepted per validity transaction.
    ///
    /// Capped at [`DEFAULT_MAX_VALIDITY_PREDICATES`], the fixed wire ceiling the
    /// request deserializer enforces. Values above it can never be honored and
    /// are rejected at startup rather than silently truncated.
    #[arg(
        long = "builder.validity-max-predicates",
        default_value_t = DEFAULT_MAX_VALIDITY_PREDICATES,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new()
            .range(1..=DEFAULT_MAX_VALIDITY_PREDICATES as u64),
    )]
    pub validity_max_predicates: usize,

    /// Staged validity signature rollout policy for raw and forwarded submissions.
    #[arg(long = "validity-signature-mode", default_value_t = ValiditySignatureMode::Off)]
    pub validity_signature_mode: ValiditySignatureMode,

    /// Hard cutoff, in milliseconds, on cumulative validity-predicate evaluation time per
    /// payload build. Once exceeded, further validity-gated transactions are deferred to a
    /// later build rather than evaluated.
    #[arg(long = "builder.predicate-eval-hard-cutoff-ms", default_value = "10")]
    pub predicate_eval_hard_cutoff_ms: u64,

    /// Parked predicate bucket depth at which state wakeups become threshold-aware.
    #[arg(
        long = "builder.predicate-bucket-ordered-threshold",
        default_value = "32",
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    pub predicate_bucket_ordered_threshold: usize,

    /// Hold back validity transactions whose predicate was found unsatisfied until a state change
    /// wakes them: off, shadow (track and count mismatches only), or enforce. Resting transactions
    /// carry across blocks this builder finishes in sequence.
    #[arg(long = "builder.resting-predicates", value_enum, default_value = "off")]
    pub resting_predicate_mode: RestingPredicateMode,

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

    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution. Disable with
    /// `--builder.eip8130-manifest-precheck=false`.
    #[arg(
        long = "builder.eip8130-manifest-precheck",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub manifest_precheck_enabled: bool,

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
            self.enable_resource_metering,
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
            enable_resource_metering: false,
            resource_metering_schedule: None,
            validity_max_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
            validity_signature_mode: ValiditySignatureMode::Off,
            predicate_eval_hard_cutoff_ms: 10,
            predicate_bucket_ordered_threshold: 32,
            resting_predicate_mode: RestingPredicateMode::Off,
            tx_data_store_buffer_size: 10000,
            metering_store_ttl_secs: 30,
            rejection_cache_max_capacity: 100_000,
            rejection_cache_ttl_secs: 1800,
            manifest_precheck_enabled: true,
            transaction_events: TransactionEventsArgs::default(),
            shadow_indexer: ShadowIndexerArgs::default(),
        }
    }
}

impl Args {
    /// Builds configuration for the builder transaction insertion RPC.
    pub const fn builder_api_config(&self) -> BuilderApiExtensionConfig {
        BuilderApiExtensionConfig::new(self.validity_max_predicates)
            .with_validity_signature_mode(self.validity_signature_mode)
    }

    /// Converts these CLI arguments into a [`BuilderConfig`] using the given shared metering
    /// provider. The same provider must also be passed to the RPC extension so that the
    /// building loop and the `base_setMeteringInformation` handler share a single store.
    pub fn into_builder_config(
        self,
        metering_provider: SharedMeteringProvider,
    ) -> eyre::Result<BuilderConfig> {
        if self.enable_resource_metering && self.resource_metering_schedule.is_none() {
            warn!(
                "--builder.enable-resource-metering (BUILDER_ENABLE_RESOURCE_METERING) is enabled without --payload.resource-metering-schedule (PAYLOAD_RESOURCE_METERING_SCHEDULE); native payload resource-metering schedule evaluation is inactive"
            );
        }

        let resource_metering = ResourceMeteringConfig::from_parts(
            self.enable_resource_metering,
            self.resource_metering_schedule.as_deref(),
            Arc::clone(&metering_provider),
        )
        .wrap_err_with(|| {
            self.resource_metering_schedule.as_ref().map_or_else(
                || "failed to configure resource metering".to_string(),
                |path| format!("failed to load resource metering schedule from {}", path.display()),
            )
        })?;
        if let Some(path) = self.resource_metering_schedule.as_deref()
            && resource_metering.is_active()
        {
            info!(
                path = %path.display(),
                dimensions = resource_metering.schedule.dimensions.len(),
                "native resource metering schedule loaded"
            );
        }

        Ok(BuilderConfig {
            state_provider_metrics: false,
            block_time: Duration::from_millis(self.chain_block_time),
            da_config: Default::default(),
            gas_limit_config: Default::default(),
            predicate_eval_hard_cutoff: Duration::from_millis(self.predicate_eval_hard_cutoff_ms),
            predicate_bucket_ordered_threshold: self.predicate_bucket_ordered_threshold,
            resting_predicate_mode: self.resting_predicate_mode,
            metering_provider,
            resource_metering,
            rejection_cache: RejectionCache::new(
                self.rejection_cache_max_capacity,
                Duration::from_secs(self.rejection_cache_ttl_secs),
            ),
            manifest_precheck_enabled: self.manifest_precheck_enabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, TxHash, U256};
    use base_builder_core::{MeteringProvider, NoopMeteringProvider};
    use base_bundles::MeterBundleResponse;
    use clap::{CommandFactory, Parser};
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
        assert_eq!(args.validity_max_predicates, DEFAULT_MAX_VALIDITY_PREDICATES);
        let config = convert(args);
        assert_eq!(config.block_time, Duration::from_millis(1000));
        assert!(config.manifest_precheck_enabled);
    }

    #[test]
    fn validity_max_predicates_rejects_values_above_the_wire_ceiling() {
        // The request deserializer bounds batches at DEFAULT_MAX_VALIDITY_PREDICATES,
        // so a larger configured maximum could never be honored. Reject it at
        // startup instead of silently accepting an unenforceable limit.
        let error = CommandParser::try_parse_from([
            "builder",
            "--builder.validity-max-predicates",
            &(DEFAULT_MAX_VALIDITY_PREDICATES + 1).to_string(),
        ])
        .expect_err("a maximum above the wire ceiling should be rejected");

        assert!(error.to_string().contains("--builder.validity-max-predicates"));
    }

    #[test]
    fn validity_max_predicates_rejects_zero() {
        let error =
            CommandParser::try_parse_from(["builder", "--builder.validity-max-predicates", "0"])
                .expect_err("a maximum of zero should be rejected");

        assert!(error.to_string().contains("--builder.validity-max-predicates"));
    }

    #[rstest]
    #[case::off("off", ValiditySignatureMode::Off)]
    #[case::verify_if_present("verify-if-present", ValiditySignatureMode::VerifyIfPresent)]
    #[case::required("required", ValiditySignatureMode::Required)]
    fn builder_api_config_preserves_validity_settings(
        #[case] mode: &str,
        #[case] expected: ValiditySignatureMode,
    ) {
        let parsed = CommandParser::parse_from([
            "builder",
            "--builder.validity-max-predicates",
            "7",
            "--validity-signature-mode",
            mode,
        ]);
        let config = parsed.args.builder_api_config();

        assert_eq!(config.max_validity_predicates, 7);
        assert_eq!(config.validity_signature_mode, expected);
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

    #[rstest]
    #[case::block_time_1s(1000, 1000)]
    #[case::block_time_2s(2000, 2000)]
    #[case::block_time_250ms(250, 250)]
    fn chain_block_time_maps_to_block_time(#[case] input_ms: u64, #[case] expected_ms: u64) {
        let args = Args { chain_block_time: input_ms, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.block_time, Duration::from_millis(expected_ms));
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
    #[case::default(10, Duration::from_millis(10))]
    #[case::zero(0, Duration::from_millis(0))]
    #[case::custom(25, Duration::from_millis(25))]
    fn predicate_eval_hard_cutoff_maps_correctly(#[case] input: u64, #[case] expected: Duration) {
        let args = Args { predicate_eval_hard_cutoff_ms: input, ..Default::default() };
        let config = convert(args);
        assert_eq!(config.predicate_eval_hard_cutoff, expected);
    }

    #[test]
    fn predicate_bucket_threshold_is_nonzero_and_propagated() {
        let config = convert(Args { predicate_bucket_ordered_threshold: 64, ..Default::default() });
        assert_eq!(config.predicate_bucket_ordered_threshold, 64);
        assert!(
            CommandParser::try_parse_from([
                "builder",
                "--builder.predicate-bucket-ordered-threshold",
                "0",
            ])
            .is_err()
        );
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

    #[rstest]
    #[case::default(&[], RestingPredicateMode::Off)]
    #[case::shadow(&["--builder.resting-predicates", "shadow"], RestingPredicateMode::Shadow)]
    #[case::enforce(&["--builder.resting-predicates", "enforce"], RestingPredicateMode::Enforce)]
    fn resting_predicate_mode_is_propagated(
        #[case] flags: &[&str],
        #[case] expected: RestingPredicateMode,
    ) {
        let args =
            CommandParser::parse_from(std::iter::once("builder").chain(flags.iter().copied())).args;
        assert_eq!(convert(args).resting_predicate_mode, expected);
    }

    const VALID_RESOURCE_METERING_SCHEDULE_JSON: &str = r#"{
        "version": 1,
        "dimensions": [{
            "name": "compute",
            "blockLimit": 1000,
            "transactionLimit": 500,
            "baseGasWeight": 1,
            "operations": [
                {"name": "SSTORE", "gasUsedWeight": 2, "countCost": 10},
                {"name": "STATE_CHANGED_STORAGE_SLOT", "countCost": 25}
            ],
            "dryRun": true
        }]
    }"#;

    #[test]
    fn parses_resource_metering_schedule_flag_and_env() {
        let parsed = CommandParser::try_parse_from([
            "builder",
            "--builder.enable-resource-metering",
            "--payload.resource-metering-schedule",
            "/tmp/resource-metering-schedule.json",
        ])
        .expect("schedule CLI flag should be accepted");

        assert!(parsed.args.enable_resource_metering);
        assert_eq!(
            parsed.args.resource_metering_schedule.as_deref(),
            Some(std::path::Path::new("/tmp/resource-metering-schedule.json"))
        );

        let cmd = CommandParser::command();
        let schedule_arg = cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some("payload.resource-metering-schedule"))
            .expect("schedule argument should be registered");
        assert_eq!(
            schedule_arg.get_env(),
            Some(std::ffi::OsStr::new("PAYLOAD_RESOURCE_METERING_SCHEDULE"))
        );
    }

    #[test]
    fn default_args_leave_resource_metering_disabled_with_empty_schedule() {
        let config = convert(Args::default());
        assert!(!config.resource_metering.enabled);
        assert!(config.resource_metering.schedule.is_empty());
        assert!(!config.resource_metering.is_active());

        let enabled_without_schedule =
            convert(Args { enable_resource_metering: true, ..Default::default() });
        assert!(enabled_without_schedule.resource_metering.enabled);
        assert!(enabled_without_schedule.resource_metering.schedule.is_empty());
        assert!(!enabled_without_schedule.resource_metering.is_active());

        let dir = tempfile::tempdir().expect("tempdir");
        let schedule_path = dir.path().join("schedule.json");
        std::fs::write(&schedule_path, VALID_RESOURCE_METERING_SCHEDULE_JSON)
            .expect("write schedule");
        let disabled_with_schedule = convert(Args {
            enable_resource_metering: false,
            resource_metering_schedule: Some(schedule_path),
            ..Default::default()
        });
        assert!(!disabled_with_schedule.resource_metering.enabled);
        assert!(disabled_with_schedule.resource_metering.schedule.is_empty());
        assert!(!disabled_with_schedule.resource_metering.is_active());
    }

    #[test]
    fn into_builder_config_loads_schedule_when_resource_metering_is_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schedule_path = dir.path().join("schedule.json");
        std::fs::write(&schedule_path, VALID_RESOURCE_METERING_SCHEDULE_JSON)
            .expect("write schedule");

        let provider: SharedMeteringProvider =
            Arc::new(MeteringStore::new(true, 100, Duration::from_secs(30)));
        let args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: Some(schedule_path),
            ..Default::default()
        };

        let config =
            args.into_builder_config(Arc::clone(&provider)).expect("valid schedule should convert");
        assert!(config.resource_metering.enabled);
        assert!(config.resource_metering.is_active());
        assert_eq!(config.resource_metering.schedule.dimensions.len(), 1);
        assert_eq!(config.resource_metering.schedule.dimensions[0].name, "compute");
        assert!(Arc::ptr_eq(&config.resource_metering.provider, &provider));
    }

    #[test]
    fn into_builder_config_rejects_invalid_or_missing_schedule_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_path = dir.path().join("missing.json");
        let missing_display = missing_path.display().to_string();
        let missing_args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: Some(missing_path),
            ..Default::default()
        };
        let missing_err = missing_args
            .into_builder_config(Arc::new(NoopMeteringProvider))
            .expect_err("missing schedule file should fail startup conversion");
        assert!(
            missing_err.to_string().contains(&missing_display),
            "error should name the schedule path: {missing_err:#}"
        );

        let invalid_path = dir.path().join("invalid.json");
        let invalid_display = invalid_path.display().to_string();
        std::fs::write(&invalid_path, "{ not valid json").expect("write invalid json");
        let invalid_args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: Some(invalid_path),
            ..Default::default()
        };
        let invalid_err = invalid_args
            .into_builder_config(Arc::new(NoopMeteringProvider))
            .expect_err("invalid schedule JSON should fail startup conversion");
        assert!(
            invalid_err.to_string().contains(&invalid_display),
            "error should name the schedule path: {invalid_err:#}"
        );

        let empty_path = dir.path().join("empty.json");
        let empty_display = empty_path.display().to_string();
        std::fs::write(&empty_path, r#"{"version": 1, "dimensions": []}"#)
            .expect("write empty schedule");
        let empty_args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: Some(empty_path),
            ..Default::default()
        };
        let empty_err = empty_args
            .into_builder_config(Arc::new(NoopMeteringProvider))
            .expect_err("empty schedule dimensions should fail closed when metering is enabled");
        assert!(
            empty_err.to_string().contains(&empty_display),
            "error should name the schedule path: {empty_err:#}"
        );
    }

    #[test]
    fn enable_resource_metering_is_only_switch_and_supports_env() {
        let cmd = CommandParser::command();
        let enable_arg = cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some("builder.enable-resource-metering"))
            .expect("enable-resource-metering argument should be registered");
        assert_eq!(
            enable_arg.get_env(),
            Some(std::ffi::OsStr::new("BUILDER_ENABLE_RESOURCE_METERING"))
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let schedule_path = dir.path().join("schedule.json");
        std::fs::write(&schedule_path, VALID_RESOURCE_METERING_SCHEDULE_JSON)
            .expect("write schedule");

        let off_args = Args {
            enable_resource_metering: false,
            resource_metering_schedule: Some(schedule_path.clone()),
            ..Default::default()
        };
        let store = off_args.build_metering_store();
        assert!(
            !store.is_enabled(),
            "MeteringStore must stay disabled when --builder.enable-resource-metering is false"
        );
        let config =
            off_args.into_builder_config(Arc::new(store)).expect("conversion should succeed");
        assert!(!config.resource_metering.enabled);
        assert!(!config.resource_metering.is_active());

        let on_args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: Some(schedule_path),
            ..Default::default()
        };
        let on_store = on_args.build_metering_store();
        assert!(on_store.is_enabled());
        let on_config =
            on_args.into_builder_config(Arc::new(on_store)).expect("conversion should succeed");
        assert!(on_config.resource_metering.enabled);
        assert!(on_config.resource_metering.is_active());
    }

    #[test]
    fn live_enabled_without_schedule_configs_boot_with_warning_metric() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let args = Args {
            enable_resource_metering: true,
            resource_metering_schedule: None,
            ..Default::default()
        };
        let store = args.build_metering_store();
        assert!(store.is_enabled());
        let provider: SharedMeteringProvider = Arc::new(store);
        // `base sequencer` performs `into_builder_config` before Reth installs its
        // recorder during launch; record the counter from the post-install startup
        // hook so it is not lost there.
        let config = args
            .into_builder_config(provider)
            .expect("live enabled-without-schedule config must boot without error");
        metrics::with_local_recorder(&recorder, || {
            config.record_startup_metrics();
        });
        assert!(config.resource_metering.enabled);
        assert!(!config.resource_metering.is_active());
        let rendered = handle.render();
        assert!(rendered.contains("base_builder_resource_metering_schedule_missing_total 1"));
    }
}
