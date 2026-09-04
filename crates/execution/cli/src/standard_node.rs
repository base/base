//! Standard Base execution-node arguments and runner wiring.

use std::{env, path::PathBuf, sync::Arc, time::Duration};

use base_builder_metering::{
    DEFAULT_METERING_STORE_MAX_CAPACITY, DEFAULT_METERING_STORE_TTL_SECS, MeteringStore,
    MeteringStoreExtension,
};
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_execution_payload_builder::{
    NoopMeteringProvider, REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL, RejectionCache,
    ResourceMeteringConfig, SharedMeteringProvider,
};
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_metering::{MeteredOpcodes, MeteringConfig, MeteringExtension};
use base_node_core::{HasRollupArgs, RollupArgs};
use base_node_runner::{BaseNodeBuilder, BaseNodeRunner, LaunchedBaseNode, PayloadServiceBuilder};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY,
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use base_proofs_extension::ProofsHistoryExtension;
use base_shadow_indexer::{ShadowIndexerConfig, ShadowIndexerExtension, ShadowRetentionConfig};
use base_shadow_indexer_db::{
    DEFAULT_DATABASE, DEFAULT_PORT, DEFAULT_USERNAME, PgConnectionParams, ShadowDbConfig,
};
use base_tx_forwarding::{
    DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS, DEFAULT_RESEND_AFTER_MS, TxForwardingConfig,
    TxForwardingExtension,
};
use base_txpool_rpc::{
    DEFAULT_MAX_VALIDITY_PREDICATES, SendRawTransactionValidityExtension, TxPoolRpcConfig,
    TxPoolRpcExtension,
};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};
use base_upgrade_signal::UpgradeSignalStartupMode;
use tracing::warn;
use url::Url;

use crate::upgrade_signal::{
    ExecutionUpgradeSignal, ExecutionUpgradeSignalConfig, ExecutionUpgradeSignalRuntimeExtension,
};

/// CLI arguments for metering RPC.
#[derive(Debug, Clone, PartialEq, Eq, Default, clap::Args)]
pub struct MeteringArgs {
    /// Enable metering RPC for transaction bundle simulation.
    ///
    /// Native kill switch for payload resource metering: a loaded schedule is
    /// evaluated only when this is set. The Flashblocks builder uses
    /// `--builder.enable-resource-metering` instead.
    #[arg(long = "enable-metering", env = "ENABLE_METERING", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Comma-separated list of EVM opcodes to track for gas metering
    /// (e.g., "SSTORE,SLOAD,KECCAK256"). Precompile gas is always tracked.
    #[arg(long = "metering.metered-opcodes", requires = "enable_metering", value_delimiter = ',')]
    pub metering_metered_opcodes: Vec<String>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(long = "metering.gas-limit", requires = "enable_metering", hide = true)]
    pub metering_gas_limit: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(long = "metering.execution-time-us", requires = "enable_metering", hide = true)]
    pub metering_execution_time_us: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(long = "metering.state-root-time-us", requires = "enable_metering", hide = true)]
    pub metering_state_root_time_us: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(long = "metering.da-bytes", requires = "enable_metering", hide = true)]
    pub metering_da_bytes: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(
        long = "metering.target-flashblocks-per-block",
        requires = "enable_metering",
        hide = true
    )]
    pub metering_target_flashblocks_per_block: Option<usize>,

    /// Resource-metering schedule. Evaluated when `--enable-metering` is set.
    #[command(flatten)]
    pub resource_metering: ResourceMeteringArgs,
}

/// CLI arguments for payload resource metering.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ResourceMeteringArgs {
    /// JSON file containing the startup resource-metering schedule.
    ///
    /// Resource metering runs when `--enable-metering` is set and this schedule
    /// is non-empty. Per-dimension `dryRun` in the file observes a budget
    /// without excluding transactions.
    #[arg(long = "payload.resource-metering-schedule", env = "PAYLOAD_RESOURCE_METERING_SCHEDULE")]
    pub resource_metering_schedule: Option<PathBuf>,

    /// Maximum number of permanently rejected transaction hashes retained by the
    /// native payload builder.
    #[arg(
        long = "payload.rejection-cache-max-capacity",
        default_value_t = REJECTION_CACHE_MAX_CAPACITY
    )]
    pub rejection_cache_max_capacity: u64,

    /// TTL in seconds for native payload rejection-cache entries.
    #[arg(
        long = "payload.rejection-cache-ttl-secs",
        default_value_t = REJECTION_CACHE_TTL.as_secs()
    )]
    pub rejection_cache_ttl_secs: u64,
}

impl Default for ResourceMeteringArgs {
    fn default() -> Self {
        Self {
            resource_metering_schedule: None,
            rejection_cache_max_capacity: REJECTION_CACHE_MAX_CAPACITY,
            rejection_cache_ttl_secs: REJECTION_CACHE_TTL.as_secs(),
        }
    }
}

/// Default maximum number of open shadow indexer database connections.
const DEFAULT_SHADOW_INDEXER_MAX_CONNECTIONS: u32 = 5;
/// Default timeout when acquiring a shadow indexer database connection.
const DEFAULT_SHADOW_INDEXER_CONNECTION_TIMEOUT: &str = "30s";
/// Default age at which persisted shadow blocks are deleted.
const DEFAULT_SHADOW_INDEXER_RETENTION: &str = "30d";
/// Default delay between shadow block retention sweeps.
const DEFAULT_SHADOW_INDEXER_RETENTION_INTERVAL: &str = "1h";

/// CLI arguments for the shadow indexer `ExEx` that persists committed execution blocks.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ShadowIndexerArgs {
    /// Enable the shadow indexer `ExEx` that persists committed execution blocks to Postgres.
    #[arg(long = "enable-shadow-indexer", env = "ENABLE_SHADOW_INDEXER")]
    pub enable_shadow_indexer: bool,

    /// Host of the shadow indexer database.
    #[arg(
        long = "shadow-indexer.db-host",
        env = "SHADOW_INDEXER_DB_HOST",
        value_name = "SHADOW_INDEXER_DB_HOST",
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_db_host: Option<String>,

    /// Password for the shadow indexer database role.
    #[arg(
        long = "shadow-indexer.db-password",
        env = "SHADOW_INDEXER_DB_PASSWORD",
        value_name = "SHADOW_INDEXER_DB_PASSWORD",
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_db_password: Option<String>,

    /// Port of the shadow indexer database.
    #[arg(
        long = "shadow-indexer.db-port",
        env = "SHADOW_INDEXER_DB_PORT",
        default_value_t = DEFAULT_PORT,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_db_port: u16,

    /// Name of the shadow indexer database.
    #[arg(
        long = "shadow-indexer.db-name",
        env = "SHADOW_INDEXER_DB_NAME",
        default_value = DEFAULT_DATABASE,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_db_name: String,

    /// Role to authenticate to the shadow indexer database as.
    #[arg(
        long = "shadow-indexer.db-user",
        env = "SHADOW_INDEXER_DB_USER",
        default_value = DEFAULT_USERNAME,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_db_user: String,

    /// Maximum number of open shadow indexer database connections.
    #[arg(
        long = "shadow-indexer.max-connections",
        env = "SHADOW_INDEXER_MAX_CONNECTIONS",
        default_value_t = DEFAULT_SHADOW_INDEXER_MAX_CONNECTIONS,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_max_connections: u32,

    /// Timeout when acquiring a shadow indexer database connection.
    #[arg(
        long = "shadow-indexer.connection-timeout",
        env = "SHADOW_INDEXER_CONNECTION_TIMEOUT",
        default_value = DEFAULT_SHADOW_INDEXER_CONNECTION_TIMEOUT,
        value_parser = humantime::parse_duration,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_connection_timeout: Duration,

    /// Age at which persisted shadow blocks are deleted, measured from their last write.
    #[arg(
        long = "shadow-indexer.retention",
        env = "SHADOW_INDEXER_RETENTION",
        default_value = DEFAULT_SHADOW_INDEXER_RETENTION,
        value_parser = humantime::parse_duration,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_retention: Duration,

    /// Delay between shadow block retention sweeps.
    #[arg(
        long = "shadow-indexer.retention-interval",
        env = "SHADOW_INDEXER_RETENTION_INTERVAL",
        default_value = DEFAULT_SHADOW_INDEXER_RETENTION_INTERVAL,
        value_parser = humantime::parse_duration,
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_retention_interval: Duration,
}

impl Default for ShadowIndexerArgs {
    fn default() -> Self {
        Self {
            enable_shadow_indexer: false,
            shadow_indexer_db_host: None,
            shadow_indexer_db_password: None,
            shadow_indexer_db_port: DEFAULT_PORT,
            shadow_indexer_db_name: DEFAULT_DATABASE.to_string(),
            shadow_indexer_db_user: DEFAULT_USERNAME.to_string(),
            shadow_indexer_max_connections: DEFAULT_SHADOW_INDEXER_MAX_CONNECTIONS,
            shadow_indexer_connection_timeout: humantime::parse_duration(
                DEFAULT_SHADOW_INDEXER_CONNECTION_TIMEOUT,
            )
            .expect("valid default shadow indexer connection timeout"),
            shadow_indexer_retention: humantime::parse_duration(DEFAULT_SHADOW_INDEXER_RETENTION)
                .expect("valid default shadow indexer retention"),
            shadow_indexer_retention_interval: humantime::parse_duration(
                DEFAULT_SHADOW_INDEXER_RETENTION_INTERVAL,
            )
            .expect("valid default shadow indexer retention interval"),
        }
    }
}

/// CLI arguments for a standard Base execution node.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct StandardNodeArgs {
    /// Shared execution node arguments.
    #[command(flatten)]
    pub rpc: RpcStandardNodeArgs,

    /// Metering RPC and priority-fee resource budget arguments.
    #[command(flatten)]
    pub metering: MeteringArgs,

    /// Shadow indexer `ExEx` arguments.
    #[command(flatten)]
    pub shadow_indexer: ShadowIndexerArgs,
}

/// CLI arguments for a Base execution node embedded by the unified RPC command.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct RpcStandardNodeArgs {
    /// Rollup arguments.
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// RPC endpoint used to forward submitted transactions without enabling sequencer mode.
    #[arg(
        long = "rpc.forwarding-endpoint",
        env = "OP_RETH_SEQUENCER_HTTP",
        value_name = "RPC_FORWARDING_ENDPOINT"
    )]
    pub rpc_forwarding_endpoint: Option<String>,

    /// A URL pointing to a secure websocket subscription that streams out flashblocks.
    ///
    /// If given, the flashblocks are received to build pending block. All request with "pending"
    /// block tag will use the pending state based on flashblocks.
    #[arg(long, alias = "websocket-url")]
    pub flashblocks_url: Option<Url>,

    /// The max pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Interval between flashblocks upstream websocket ping frames.
    #[arg(
        long = "flashblocks.ping-interval",
        value_name = "FLASHBLOCKS_PING_INTERVAL",
        default_value = "30s",
        value_parser = humantime::parse_duration,
        requires = "flashblocks_url"
    )]
    pub flashblocks_ping_interval: Duration,

    /// Enable transaction tracing for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable durable transaction event journal emission from txpool tracing.
    #[arg(
        long = "enable-transaction-event-journal",
        value_name = "ENABLE_TRANSACTION_EVENT_JOURNAL"
    )]
    pub enable_transaction_event_journal: bool,

    /// Dedicated JSONL path for durable transaction event journal emission.
    #[arg(
        long = "transaction-event-journal-path",
        value_name = "TRANSACTION_EVENT_JOURNAL_PATH",
        requires = "enable_transaction_event_journal"
    )]
    pub transaction_event_journal_path: Option<PathBuf>,

    /// Enable transaction forwarding for mempool nodes to builder RPC endpoints
    #[arg(
        long = "enable-tx-forwarding",
        value_name = "ENABLE_TX_FORWARDING",
        requires = "builder_rpc_urls"
    )]
    pub enable_tx_forwarding: bool,

    /// Enable the experimental validity transaction RPC.
    ///
    /// When transaction forwarding is enabled, validity predicates are forwarded to builders, which
    /// evaluate and enforce them during block construction. This can also be enabled on a standalone
    /// sequencer (e.g. a local devnet) that builds blocks itself, in which case forwarding is not
    /// required.
    #[arg(long = "enable-experimental-validity-transactions")]
    pub enable_experimental_validity_transactions: bool,

    /// Maximum validity predicates accepted per experimental transaction.
    #[arg(
        long = "experimental-validity-max-predicates",
        default_value_t = DEFAULT_MAX_VALIDITY_PREDICATES,
        requires = "enable_experimental_validity_transactions"
    )]
    pub experimental_validity_max_predicates: usize,

    /// Builder RPC endpoints for transaction forwarding (one forwarder per URL), used by mempool nodes
    #[arg(
        long = "builder-rpc-urls",
        value_name = "BUILDER_RPC_URLS",
        value_delimiter = ',',
        requires = "enable_tx_forwarding"
    )]
    pub builder_rpc_urls: Vec<Url>,

    /// Resend transactions that haven't been included after this duration in ms (default: 2 blocks)
    #[arg(
        long = "tx-forwarding-resend-after-ms",
        value_name = "TX_FORWARDING_RESEND_AFTER_MS",
        default_value_t = DEFAULT_RESEND_AFTER_MS,
        requires = "enable_tx_forwarding"
    )]
    pub tx_forwarding_resend_after_ms: u64,

    /// Maximum number of transactions per forwarding batch
    #[arg(
        long = "tx-forwarding-batch-size",
        value_name = "TX_FORWARDING_BATCH_SIZE",
        default_value_t = DEFAULT_MAX_BATCH_SIZE,
        requires = "enable_tx_forwarding"
    )]
    pub tx_forwarding_batch_size: usize,

    /// Maximum RPC requests per second per forwarder (0 = unlimited).
    #[arg(
        long = "tx-forwarding-max-rps",
        value_name = "TX_FORWARDING_MAX_RPS",
        default_value_t = DEFAULT_MAX_RPS,
        requires = "enable_tx_forwarding"
    )]
    pub tx_forwarding_max_rps: u32,
}

impl From<RpcStandardNodeArgs> for StandardNodeArgs {
    fn from(mut args: RpcStandardNodeArgs) -> Self {
        if args.rollup_args.sequencer.is_none() {
            args.rollup_args.sequencer.clone_from(&args.rpc_forwarding_endpoint);
        }

        Self {
            rpc: args,
            metering: MeteringArgs::default(),
            shadow_indexer: ShadowIndexerArgs::default(),
        }
    }
}

impl StandardNodeArgs {
    /// Sets the metering arguments on this standard node configuration.
    pub fn with_metering(mut self, metering: MeteringArgs) -> Self {
        self.metering = metering;
        self
    }

    /// Sets the shadow indexer arguments on this standard node configuration.
    pub fn with_shadow_indexer(mut self, shadow_indexer: ShadowIndexerArgs) -> Self {
        self.shadow_indexer = shadow_indexer;
        self
    }
}

impl TryFrom<&ShadowIndexerArgs> for ShadowIndexerConfig {
    type Error = eyre::Error;

    fn try_from(args: &ShadowIndexerArgs) -> eyre::Result<Self> {
        let connection = if args.enable_shadow_indexer {
            let require = |value: &Option<String>, flag: &str, env: &str| {
                value.clone().ok_or_else(|| {
                    eyre::eyre!(
                        "--enable-shadow-indexer (env ENABLE_SHADOW_INDEXER) requires {flag} (env \
                         {env})"
                    )
                })
            };

            PgConnectionParams {
                host: require(
                    &args.shadow_indexer_db_host,
                    "--shadow-indexer.db-host",
                    "SHADOW_INDEXER_DB_HOST",
                )?,
                port: args.shadow_indexer_db_port,
                database: args.shadow_indexer_db_name.clone(),
                username: args.shadow_indexer_db_user.clone(),
                password: require(
                    &args.shadow_indexer_db_password,
                    "--shadow-indexer.db-password",
                    "SHADOW_INDEXER_DB_PASSWORD",
                )?,
            }
        } else {
            PgConnectionParams::default()
        };

        if args.enable_shadow_indexer {
            if args.shadow_indexer_retention.is_zero() {
                eyre::bail!(
                    "--shadow-indexer.retention (env SHADOW_INDEXER_RETENTION) must be non-zero"
                );
            }
            if args.shadow_indexer_retention_interval.is_zero() {
                eyre::bail!(
                    "--shadow-indexer.retention-interval \
                     (env SHADOW_INDEXER_RETENTION_INTERVAL) must be non-zero"
                );
            }
        }

        Ok(Self {
            enabled: args.enable_shadow_indexer,
            db: ShadowDbConfig {
                connection,
                max_connections: args.shadow_indexer_max_connections,
                connection_timeout: args.shadow_indexer_connection_timeout,
            },
            builder_version: env!("CARGO_PKG_VERSION").to_string(),
            retention: ShadowRetentionConfig {
                period: args.shadow_indexer_retention,
                interval: args.shadow_indexer_retention_interval,
            },
        })
    }
}

impl HasRollupArgs for StandardNodeArgs {
    fn rollup_args(&self) -> &RollupArgs {
        &self.rpc.rollup_args
    }
}

impl From<&StandardNodeArgs> for Option<FlashblocksConfig> {
    fn from(args: &StandardNodeArgs) -> Self {
        args.rpc.flashblocks_url.clone().map(|url| {
            FlashblocksConfig::new(url, args.rpc.max_pending_blocks_depth)
                .with_subscriber_ping_interval(args.rpc.flashblocks_ping_interval)
        })
    }
}

impl From<&StandardNodeArgs> for TxForwardingConfig {
    fn from(args: &StandardNodeArgs) -> Self {
        if !args.rpc.enable_tx_forwarding || args.rpc.builder_rpc_urls.is_empty() {
            return Self::default();
        }

        Self::new(args.rpc.builder_rpc_urls.clone())
            .with_resend_after_ms(args.rpc.tx_forwarding_resend_after_ms)
            .with_max_batch_size(args.rpc.tx_forwarding_batch_size)
            .with_max_rps(args.rpc.tx_forwarding_max_rps)
    }
}

/// Standard Base execution-node runner wiring.
#[derive(Debug, Clone, Copy)]
pub struct StandardBaseRethNode;

impl StandardBaseRethNode {
    /// Applies a configured L1 upgrade signal to the execution chain spec before startup.
    pub async fn apply_initial_upgrade_signal<A: HasRollupArgs + ?Sized>(
        builder: BaseNodeBuilder,
        args: &A,
    ) -> eyre::Result<BaseNodeBuilder> {
        Self::apply_initial_upgrade_signal_from_rollup_args(builder, args.rollup_args()).await
    }

    /// Applies a configured L1 upgrade signal from rollup args before startup.
    pub async fn apply_initial_upgrade_signal_from_rollup_args(
        builder: BaseNodeBuilder,
        rollup_args: &RollupArgs,
    ) -> eyre::Result<BaseNodeBuilder> {
        Self::apply_initial_upgrade_signal_from_rollup_args_with_startup_mode(
            builder,
            rollup_args,
            UpgradeSignalStartupMode::ReadAndApply,
        )
        .await
    }

    /// Applies a configured L1 upgrade signal from rollup args with explicit startup behavior.
    pub async fn apply_initial_upgrade_signal_from_rollup_args_with_startup_mode(
        mut builder: BaseNodeBuilder,
        rollup_args: &RollupArgs,
        startup_mode: UpgradeSignalStartupMode,
    ) -> eyre::Result<BaseNodeBuilder> {
        let Some(config) = Self::upgrade_signal_config(rollup_args)? else {
            return Ok(builder);
        };
        if !startup_mode.reads_and_applies() || !config.signal_config.mode.applies_at_startup() {
            return Ok(builder);
        }

        let chain_spec = Arc::make_mut(&mut builder.config_mut().chain);
        ExecutionUpgradeSignal::apply_initial_signal_to_chain_spec(&config, chain_spec).await?;

        Ok(builder)
    }

    /// Installs the upgrade signal runtime extension when execution-side live reads are configured.
    pub fn install_upgrade_signal_runtime_extension<SB: PayloadServiceBuilder>(
        runner: &mut BaseNodeRunner<SB>,
        rollup_args: &RollupArgs,
    ) -> eyre::Result<()> {
        let Some(config) = Self::upgrade_signal_config(rollup_args)? else {
            return Ok(());
        };

        runner.install_ext::<ExecutionUpgradeSignalRuntimeExtension>(config);

        Ok(())
    }

    /// Validates execution upgrade signal arguments before node setup.
    ///
    /// Execution upgrade-signal polling is configured independently from consensus polling, so a
    /// configured upgrade-signal contract always requires an explicit `--upgrade-signal.l1-rpc` for
    /// its startup application, runtime admin refresh, and live metrics observer.
    pub fn validate_upgrade_signal_args(rollup_args: &RollupArgs) -> eyre::Result<()> {
        if rollup_args.upgrade_signal.config().is_some()
            && rollup_args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.is_none()
        {
            eyre::bail!(
                "--upgrade-signal.contract (env BASE_NODE_UPGRADE_SIGNAL_CONTRACT) requires \
                 --upgrade-signal.l1-rpc (env BASE_NODE_UPGRADE_SIGNAL_L1_RPC) for execution \
                 upgrade-signal reads"
            );
        }

        Ok(())
    }

    fn upgrade_signal_config(
        rollup_args: &RollupArgs,
    ) -> eyre::Result<Option<ExecutionUpgradeSignalConfig>> {
        let Some(signal_config) = rollup_args.upgrade_signal.config() else {
            return Ok(None);
        };
        Self::validate_upgrade_signal_args(rollup_args)?;
        let l1_rpc = rollup_args
            .upgrade_signal_l1_rpc
            .upgrade_signal_l1_rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("execution upgrade signal L1 RPC not configured"))?;

        Ok(Some(ExecutionUpgradeSignalConfig { signal_config, l1_rpc }))
    }

    /// Builds a runner with the standard Base execution-node extensions installed.
    pub fn runner(args: StandardNodeArgs) -> eyre::Result<BaseNodeRunner> {
        let rollup_args = args.rpc.rollup_args.clone();
        // Fail fast on an incomplete upgrade-signal configuration before installing extensions.
        Self::validate_upgrade_signal_args(&rollup_args)?;
        let mut runner = BaseNodeRunner::new(rollup_args.clone());
        let resource_metering_enabled = args.metering.enable_metering;
        let provider: SharedMeteringProvider = if resource_metering_enabled
            && args.metering.resource_metering.resource_metering_schedule.is_some()
        {
            // Shared defaults with the Flashblocks builder CLI.
            let store: SharedMeteringProvider = Arc::new(MeteringStore::new(
                true,
                DEFAULT_METERING_STORE_MAX_CAPACITY as usize,
                Duration::from_secs(DEFAULT_METERING_STORE_TTL_SECS),
            ));
            runner.install_ext::<MeteringStoreExtension>(Arc::clone(&store));
            store
        } else {
            Arc::new(NoopMeteringProvider)
        };
        let resource_metering = ResourceMeteringConfig::from_parts(
            resource_metering_enabled,
            args.metering.resource_metering.resource_metering_schedule.as_deref(),
            provider,
        )?;
        let schedule_operation_names: Vec<String> =
            resource_metering.schedule.priced_operation_names().map(str::to_string).collect();
        let rejection_cache = RejectionCache::new(
            args.metering.resource_metering.rejection_cache_max_capacity,
            Duration::from_secs(args.metering.resource_metering.rejection_cache_ttl_secs),
        );
        runner =
            runner.with_resource_metering(resource_metering).with_rejection_cache(rejection_cache);

        // Create flashblocks config first so we can share its state with metering.
        let flashblocks_config: Option<FlashblocksConfig> = (&args).into();
        let transaction_event_env = TransactionEventEnv::read();
        let transaction_event_writer_config =
            transaction_event_writer_config(&args.rpc, &transaction_event_env)?;
        // Initialize before installing extensions so node-started hooks that emit
        // transaction events (e.g. tx forwarding) see a ready writer.
        if let Some(config) = transaction_event_writer_config
            && let Err(err) = GlobalTransactionEventWriter::init(Some(config))
        {
            tracing::warn!(error = %err, "transaction event journal disabled");
        }

        // Feature extensions. Several use `replace_configured` (which is overwrite,
        // not compose) on overlapping RPC methods, so install order would otherwise
        // silently decide which one wins. Coordination is enforced by self-gating:
        //   - FlashblocksExtension: registers eth_getTransactionCount (and others)
        //     iff flashblocks is enabled.
        //   - Eip8130RpcExtension: registers eth_getTransactionCount iff flashblocks
        //     is NOT (see `Eip8130RpcMode` below).
        //   - ProofsHistoryExtension: registers eth_getProof variants (disjoint from
        //     the above, so it can sit anywhere in the chain).
        // New extensions touching the same RPC methods MUST be added to this
        // coordination scheme rather than relying on install order.
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig {
            sequencer_rpc: args.rpc.rollup_args.sequencer.clone(),
        });
        runner.install_ext::<TxPoolExtension>(TxpoolConfig {
            tracing_enabled: args.rpc.enable_transaction_tracing
                || args.rpc.enable_transaction_event_journal
                || transaction_event_env.enabled,
            tracing_logs_enabled: args.rpc.enable_transaction_tracing_logs,
            transaction_event_node_role: transaction_event_node_role(),
            flashblocks_config: flashblocks_config.clone(),
        });

        if args.metering.metering_execution_time_us.is_some()
            || args.metering.metering_state_root_time_us.is_some()
            || args.metering.metering_gas_limit.is_some()
            || args.metering.metering_da_bytes.is_some()
            || args.metering.metering_target_flashblocks_per_block.is_some()
        {
            warn!("deprecated metering resource limit flags are ignored");
        }

        let metering_config = if args.metering.enable_metering {
            let opcode_names = inspector_opcode_names(
                args.metering.metering_metered_opcodes.clone(),
                schedule_operation_names,
            );
            let metered_opcodes = if opcode_names.is_empty() {
                MeteredOpcodes::default()
            } else {
                MeteredOpcodes::parse(&opcode_names)?
            }
            .with_all_precompiles();

            flashblocks_config
                .clone()
                .map_or_else(MeteringConfig::enabled, MeteringConfig::with_flashblocks)
                .with_metered_opcodes(metered_opcodes)
        } else {
            MeteringConfig::disabled()
        };
        runner.install_ext::<MeteringExtension>(metering_config);
        runner.install_ext::<ShadowIndexerExtension>((&args.shadow_indexer).try_into()?);
        let tx_forwarding_config: TxForwardingConfig = (&args).into();
        if args.rpc.enable_experimental_validity_transactions {
            runner.install_ext::<SendRawTransactionValidityExtension>(
                args.rpc.experimental_validity_max_predicates,
            );
        }
        runner.install_ext::<TxForwardingExtension>(tx_forwarding_config);
        runner.install_ext::<ProofsHistoryExtension>(rollup_args.clone());
        Self::install_upgrade_signal_runtime_extension(&mut runner, &rollup_args)?;
        let eip8130_rpc_mode = if flashblocks_config.is_some() {
            Eip8130RpcMode::Defer
        } else {
            Eip8130RpcMode::Register
        };
        runner.install_ext::<FlashblocksExtension>(flashblocks_config);
        runner.install_ext::<Eip8130RpcExtension>(eip8130_rpc_mode);
        Ok(runner)
    }

    /// Builds a standard runner with process version metrics registered on startup.
    pub fn runner_with_version_metrics(args: StandardNodeArgs) -> eyre::Result<BaseNodeRunner> {
        let mut runner = Self::runner(args)?;
        runner.add_started_callback(|| {
            base_cli_utils::register_version_metrics!();
            Ok(())
        });
        Ok(runner)
    }

    /// Launches the node and waits for it to exit.
    pub async fn run(builder: BaseNodeBuilder, args: StandardNodeArgs) -> eyre::Result<()> {
        let builder = Self::apply_initial_upgrade_signal(builder, &args).await?;

        Self::runner_with_version_metrics(args)?.run(builder).await
    }

    /// Launches the node and returns immediately with a handle.
    pub async fn launch(
        builder: BaseNodeBuilder,
        args: StandardNodeArgs,
    ) -> eyre::Result<LaunchedBaseNode> {
        Self::launch_with_upgrade_signal_startup(
            builder,
            args,
            UpgradeSignalStartupMode::ReadAndApply,
        )
        .await
    }

    /// Launches the node with explicit upgrade-signal startup behavior.
    pub async fn launch_with_upgrade_signal_startup(
        builder: BaseNodeBuilder,
        args: StandardNodeArgs,
        startup_mode: UpgradeSignalStartupMode,
    ) -> eyre::Result<LaunchedBaseNode> {
        let builder = Self::apply_initial_upgrade_signal_from_rollup_args_with_startup_mode(
            builder,
            &args.rpc.rollup_args,
            startup_mode,
        )
        .await?;

        Self::runner_with_version_metrics(args)?.launch(builder).await
    }
}

fn transaction_event_writer_config(
    args: &RpcStandardNodeArgs,
    env: &TransactionEventEnv,
) -> eyre::Result<Option<TransactionEventWriterConfig>> {
    if !args.enable_transaction_event_journal && !env.enabled {
        return Ok(None);
    }

    let file_path =
        args.transaction_event_journal_path.clone().or_else(|| env.path.clone()).ok_or_else(
            || {
                eyre::eyre!(
                    "--enable-transaction-event-journal requires --transaction-event-journal-path \
                 or BASE_TRANSACTION_EVENTS_PATH"
                )
            },
        )?;

    Ok(Some(TransactionEventWriterConfig {
        enabled: true,
        file_path,
        queue_capacity: DEFAULT_QUEUE_CAPACITY,
        max_file_bytes: env.max_file_bytes,
        max_files: env.max_files,
        required: false,
        producer: TransactionEventProducer::BaseRethNode,
        network: env.network.clone(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransactionEventEnv {
    enabled: bool,
    path: Option<PathBuf>,
    max_file_bytes: u64,
    max_files: usize,
    network: String,
}

impl TransactionEventEnv {
    fn read() -> Self {
        Self {
            enabled: transaction_event_journal_env_enabled(),
            path: env::var_os("BASE_TRANSACTION_EVENTS_PATH").map(PathBuf::from),
            max_file_bytes: env::var("BASE_TRANSACTION_EVENTS_MAX_FILE_BYTES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_MAX_FILE_BYTES),
            max_files: env::var("BASE_TRANSACTION_EVENTS_MAX_FILES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_MAX_FILES),
            network: env::var("BASE_TRANSACTION_EVENTS_NETWORK")
                .or_else(|_| env::var("BASE_NODE_NETWORK"))
                .unwrap_or_else(|_| "unknown".to_string()),
        }
    }
}

fn transaction_event_journal_env_enabled() -> bool {
    env::var("BASE_TRANSACTION_EVENTS_ENABLED").map(transaction_event_env_bool).unwrap_or(false)
}

fn transaction_event_env_bool(value: String) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

fn transaction_event_node_role() -> Option<String> {
    env::var("BASE_TRANSACTION_EVENTS_NODE_ROLE")
        .ok()
        .or_else(|| parse_otel_resource_attribute("base.node"))
}

fn parse_otel_resource_attribute(key: &str) -> Option<String> {
    env::var("OTEL_RESOURCE_ATTRIBUTES").ok().and_then(|attrs| {
        attrs.split(',').find_map(|part| {
            let (attr_key, attr_value) = part.split_once('=')?;
            (attr_key.trim() == key)
                .then(|| attr_value.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    })
}

/// Opcode and precompile names the metering inspector can parse.
///
/// Schedule `STATE_*` post-state effects are not EVM opcodes; unknown names are
/// skipped so a loaded schedule cannot fail node startup.
fn inspector_opcode_names(
    cli_names: impl IntoIterator<Item = String>,
    schedule_names: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    let mut names: Vec<String> = cli_names.into_iter().collect();
    for name in schedule_names {
        let name = name.as_ref();
        if !is_inspector_opcode_name(name) {
            continue;
        }
        if !names.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
            names.push(name.to_string());
        }
    }
    names
}

fn is_inspector_opcode_name(name: &str) -> bool {
    !name.to_ascii_uppercase().starts_with("STATE_")
        && MeteredOpcodes::parse(&[name.to_string()]).is_ok()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use clap::{Args, Parser};

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    fn default_rpc_standard_node_args() -> RpcStandardNodeArgs {
        RpcStandardNodeArgs {
            rollup_args: RollupArgs::default(),
            rpc_forwarding_endpoint: None,
            flashblocks_url: None,
            max_pending_blocks_depth: 3,
            flashblocks_ping_interval: Duration::from_secs(30),
            enable_transaction_tracing: false,
            enable_transaction_tracing_logs: false,
            enable_transaction_event_journal: false,
            transaction_event_journal_path: None,
            enable_tx_forwarding: false,
            enable_experimental_validity_transactions: false,
            experimental_validity_max_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
            builder_rpc_urls: Vec::new(),
            tx_forwarding_resend_after_ms: DEFAULT_RESEND_AFTER_MS,
            tx_forwarding_batch_size: DEFAULT_MAX_BATCH_SIZE,
            tx_forwarding_max_rps: DEFAULT_MAX_RPS,
        }
    }

    #[test]
    fn standard_node_args_provides_embedded_rollup_args() {
        let args = StandardNodeArgs::from(default_rpc_standard_node_args());
        assert!(std::ptr::eq(args.rollup_args(), &args.rpc.rollup_args));
    }

    #[test]
    fn test_flashblocks_ping_interval_defaults_to_30_seconds() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "reth",
            "--flashblocks-url",
            "wss://example.com/ws",
        ])
        .args;

        assert_eq!(args.flashblocks_ping_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_flashblocks_ping_interval_defaults_without_flashblocks_url() {
        let args = CommandParser::<RpcStandardNodeArgs>::try_parse_from(["reth"])
            .expect("default args should parse without flashblocks enabled")
            .args;

        assert_eq!(args.flashblocks_url, None);
        assert_eq!(args.flashblocks_ping_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_flashblocks_ping_interval_requires_flashblocks_url() {
        let error = CommandParser::<RpcStandardNodeArgs>::try_parse_from([
            "reth",
            "--flashblocks.ping-interval",
            "45s",
        ])
        .expect_err("ping interval should require flashblocks url");

        assert!(error.to_string().contains("--flashblocks-url"));
    }

    #[test]
    fn test_flashblocks_ping_interval_flows_into_config() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "reth",
            "--flashblocks-url",
            "wss://example.com/ws",
            "--flashblocks.ping-interval",
            "45s",
        ])
        .args;

        let standard_args = StandardNodeArgs::from(args);
        let config: FlashblocksConfig = Option::<FlashblocksConfig>::from(&standard_args)
            .expect("flashblocks config should exist");

        assert_eq!(config.subscriber_ping_interval, Duration::from_secs(45));
    }

    #[test]
    fn test_rpc_forwarding_endpoint_flows_into_standard_args() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "reth",
            "--rpc.forwarding-endpoint",
            "http://localhost:8545",
        ])
        .args;

        let standard_args = StandardNodeArgs::from(args);

        assert_eq!(
            standard_args.rpc.rollup_args.sequencer.as_deref(),
            Some("http://localhost:8545")
        );
    }

    #[test]
    fn parses_transaction_event_journal_flags() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-transaction-event-journal",
            "--transaction-event-journal-path",
            "/var/log/transaction-events/execution/events.jsonl",
        ])
        .args;

        assert!(args.enable_transaction_event_journal);
        assert_eq!(
            args.transaction_event_journal_path.as_deref(),
            Some(std::path::Path::new("/var/log/transaction-events/execution/events.jsonl"))
        );
    }

    #[test]
    fn test_rpc_forwarding_endpoint_keeps_tx_forwarding_extension_disabled() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "reth",
            "--rpc.forwarding-endpoint",
            "http://localhost:8545",
        ])
        .args;

        let standard_args = StandardNodeArgs::from(args);
        let config = TxForwardingConfig::from(&standard_args);

        assert!(!config.enabled);
        assert!(config.builder_urls.is_empty());
    }

    #[test]
    fn test_rpc_default_keeps_forwarding_disabled() {
        let standard_args = StandardNodeArgs::from(default_rpc_standard_node_args());
        let config = TxForwardingConfig::from(&standard_args);

        assert_eq!(standard_args.rpc.rollup_args.sequencer, None);
        assert!(!standard_args.rpc.enable_experimental_validity_transactions);
        assert_eq!(
            standard_args.rpc.experimental_validity_max_predicates,
            DEFAULT_MAX_VALIDITY_PREDICATES
        );
        assert!(!config.enabled);
        assert!(config.builder_urls.is_empty());
    }

    #[test]
    fn experimental_validity_transactions_parse_without_forwarding() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-experimental-validity-transactions",
        ])
        .args;

        assert!(args.rpc.enable_experimental_validity_transactions);
        assert!(!args.rpc.enable_tx_forwarding);
    }

    #[test]
    fn experimental_validity_transactions_parse_with_forwarding() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--enable-experimental-validity-transactions",
            "--experimental-validity-max-predicates",
            "8",
        ])
        .args;

        assert!(args.rpc.enable_tx_forwarding);
        assert!(args.rpc.enable_experimental_validity_transactions);
        assert_eq!(args.rpc.experimental_validity_max_predicates, 8);
        assert_eq!(args.rpc.builder_rpc_urls.len(), 1);
    }

    #[test]
    fn programmatic_validity_config_without_forwarding_is_valid() {
        let mut args = StandardNodeArgs::from(default_rpc_standard_node_args());
        args.rpc.enable_experimental_validity_transactions = true;

        StandardBaseRethNode::runner(args)
            .expect("validity transactions should not require forwarding");
    }

    #[test]
    fn test_execution_upgrade_signal_reads_require_l1_rpc() {
        let error = StandardBaseRethNode::validate_upgrade_signal_args(&RollupArgs {
            upgrade_signal: base_upgrade_signal::UpgradeSignalArgs {
                contract_address: Some(address!("0000000000000000000000000000000000000001")),
                ..Default::default()
            },
            ..Default::default()
        })
        .expect_err("execution upgrade signal reads should require an explicit execution L1 RPC");

        assert!(error.to_string().contains("--upgrade-signal.l1-rpc"));
    }

    #[test]
    fn test_standard_node_args_parses_metering_flags_once() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-metering",
            "--metering.metered-opcodes",
            "SSTORE,SLOAD",
        ])
        .args;

        assert!(args.metering.enable_metering);
        assert_eq!(
            args.metering.metering_metered_opcodes,
            vec!["SSTORE".to_string(), "SLOAD".to_string()]
        );
    }

    #[test]
    fn test_standard_node_args_parses_shadow_indexer_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-shadow-indexer",
            "--shadow-indexer.db-host",
            "shadow.example.internal",
            "--shadow-indexer.db-password",
            "hunter2",
            "--shadow-indexer.db-port",
            "6543",
            "--shadow-indexer.db-name",
            "shadow",
            "--shadow-indexer.db-user",
            "writer",
            "--shadow-indexer.max-connections",
            "9",
            "--shadow-indexer.connection-timeout",
            "45s",
            "--shadow-indexer.retention",
            "3d",
            "--shadow-indexer.retention-interval",
            "15m",
        ])
        .args;

        assert!(args.shadow_indexer.enable_shadow_indexer);
        assert_eq!(
            args.shadow_indexer.shadow_indexer_db_host.as_deref(),
            Some("shadow.example.internal")
        );
        assert_eq!(args.shadow_indexer.shadow_indexer_db_password.as_deref(), Some("hunter2"));
        assert_eq!(args.shadow_indexer.shadow_indexer_db_port, 6543);
        assert_eq!(args.shadow_indexer.shadow_indexer_db_name, "shadow");
        assert_eq!(args.shadow_indexer.shadow_indexer_db_user, "writer");
        assert_eq!(args.shadow_indexer.shadow_indexer_max_connections, 9);
        assert_eq!(args.shadow_indexer.shadow_indexer_connection_timeout, Duration::from_secs(45));
        assert_eq!(
            args.shadow_indexer.shadow_indexer_retention,
            Duration::from_secs(3 * 24 * 60 * 60)
        );
        assert_eq!(
            args.shadow_indexer.shadow_indexer_retention_interval,
            Duration::from_secs(15 * 60)
        );
    }

    #[test]
    fn test_shadow_indexer_retention_defaults_to_thirty_days() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-shadow-indexer",
            "--shadow-indexer.db-host",
            "shadow.example.internal",
            "--shadow-indexer.db-password",
            "hunter2",
        ])
        .args;

        let config = ShadowIndexerConfig::try_from(&args.shadow_indexer)
            .expect("enabled shadow indexer config should build");

        assert_eq!(config.retention.period, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(config.retention.interval, Duration::from_secs(60 * 60));
    }

    #[test]
    fn test_shadow_indexer_rejects_zero_retention() {
        let args = ShadowIndexerArgs {
            enable_shadow_indexer: true,
            shadow_indexer_db_host: Some("shadow.example.internal".to_string()),
            shadow_indexer_db_password: Some("hunter2".to_string()),
            shadow_indexer_retention: Duration::ZERO,
            ..ShadowIndexerArgs::default()
        };

        let error =
            ShadowIndexerConfig::try_from(&args).expect_err("zero retention should be rejected");

        assert!(error.to_string().contains("--shadow-indexer.retention"));
    }

    #[test]
    fn test_shadow_indexer_rejects_zero_retention_interval() {
        let args = ShadowIndexerArgs {
            enable_shadow_indexer: true,
            shadow_indexer_db_host: Some("shadow.example.internal".to_string()),
            shadow_indexer_db_password: Some("hunter2".to_string()),
            shadow_indexer_retention_interval: Duration::ZERO,
            ..ShadowIndexerArgs::default()
        };

        let error = ShadowIndexerConfig::try_from(&args)
            .expect_err("zero retention interval should be rejected");

        assert!(error.to_string().contains("--shadow-indexer.retention-interval"));
    }

    #[test]
    fn test_standard_node_args_defaults_shadow_indexer_connection_fields() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-shadow-indexer",
            "--shadow-indexer.db-host",
            "shadow.example.internal",
            "--shadow-indexer.db-password",
            "hunter2",
        ])
        .args;

        let config = ShadowIndexerConfig::try_from(&args.shadow_indexer)
            .expect("host and password are enough to build the config");

        assert_eq!(config.db.connection.port, DEFAULT_PORT);
        assert_eq!(config.db.connection.database, DEFAULT_DATABASE);
        assert_eq!(config.db.connection.username, DEFAULT_USERNAME);
    }

    #[test]
    fn test_shadow_indexer_db_host_requires_enable_flag() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "reth",
            "--shadow-indexer.db-host",
            "shadow.example.internal",
        ])
        .expect_err("shadow indexer db host should require the enable flag");

        assert!(error.to_string().contains("--enable-shadow-indexer"));
    }

    #[test]
    fn test_shadow_indexer_config_requires_db_host_when_enabled() {
        let args =
            ShadowIndexerArgs { enable_shadow_indexer: true, ..ShadowIndexerArgs::default() };
        let error = ShadowIndexerConfig::try_from(&args)
            .expect_err("enabled shadow indexer should require a db host");

        assert!(error.to_string().contains("--shadow-indexer.db-host"));
    }

    #[test]
    fn test_shadow_indexer_config_requires_db_password_when_enabled() {
        let args = ShadowIndexerArgs {
            enable_shadow_indexer: true,
            shadow_indexer_db_host: Some("shadow.example.internal".to_string()),
            ..ShadowIndexerArgs::default()
        };
        let error = ShadowIndexerConfig::try_from(&args)
            .expect_err("enabled shadow indexer should require a db password");

        assert!(error.to_string().contains("--shadow-indexer.db-password"));
    }

    #[test]
    fn test_shadow_indexer_config_disabled_by_default() {
        let config = ShadowIndexerConfig::try_from(&ShadowIndexerArgs::default())
            .expect("disabled shadow indexer config should build without connection details");

        assert!(!config.enabled);
        assert_eq!(config.builder_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_standard_node_args_parses_resource_metering_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-metering",
            "--payload.resource-metering-schedule",
            "/tmp/resource-metering.json",
        ])
        .args;

        assert!(args.metering.enable_metering);
        assert_eq!(
            args.metering.resource_metering.resource_metering_schedule.as_deref(),
            Some(std::path::Path::new("/tmp/resource-metering.json"))
        );
    }

    #[test]
    fn transaction_event_journal_requires_path_when_no_env_path_exists() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-transaction-event-journal",
            "--transaction-event-journal-path",
            "/tmp/events.jsonl",
        ])
        .args;

        let env = TransactionEventEnv {
            enabled: false,
            path: None,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            network: "unknown".to_string(),
        };
        let config = transaction_event_writer_config(&args, &env).unwrap().unwrap();
        assert_eq!(config.file_path, PathBuf::from("/tmp/events.jsonl"));
        assert_eq!(config.producer, TransactionEventProducer::BaseRethNode);
    }

    #[test]
    fn transaction_event_journal_can_be_enabled_by_env() {
        let args = CommandParser::<RpcStandardNodeArgs>::parse_from(["base-reth"]).args;
        let env = TransactionEventEnv {
            enabled: true,
            path: Some(PathBuf::from("/tmp/env-events.jsonl")),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            network: "base-devnet".to_string(),
        };
        let config = transaction_event_writer_config(&args, &env).unwrap().unwrap();

        assert_eq!(config.file_path, PathBuf::from("/tmp/env-events.jsonl"));
        assert_eq!(config.network, "base-devnet");
    }

    #[test]
    fn test_standard_node_args_accepts_deprecated_metering_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-metering",
            "--metering.execution-time-us",
            "5000000",
            "--metering.state-root-time-us",
            "1000000",
            "--metering.gas-limit",
            "30000000",
            "--metering.da-bytes",
            "1572860",
            "--metering.target-flashblocks-per-block",
            "4",
        ])
        .args;

        assert_eq!(args.metering.metering_execution_time_us, Some(5_000_000));
        assert_eq!(args.metering.metering_state_root_time_us, Some(1_000_000));
        assert_eq!(args.metering.metering_gas_limit, Some(30_000_000));
        assert_eq!(args.metering.metering_da_bytes, Some(1_572_860));
        assert_eq!(args.metering.metering_target_flashblocks_per_block, Some(4));
        assert!(args.metering.resource_metering.resource_metering_schedule.is_none());

        let config = ResourceMeteringConfig::from_parts(
            args.metering.enable_metering,
            args.metering.resource_metering.resource_metering_schedule.as_deref(),
            Arc::new(NoopMeteringProvider),
        )
        .expect("deprecated wall-clock flags must not load a schedule");
        assert!(config.schedule.is_empty());
        assert!(!config.is_active());
    }

    #[test]
    fn test_standard_node_args_parses_rejection_cache_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--payload.rejection-cache-max-capacity",
            "50",
            "--payload.rejection-cache-ttl-secs",
            "60",
        ])
        .args;

        assert_eq!(args.metering.resource_metering.rejection_cache_max_capacity, 50);
        assert_eq!(args.metering.resource_metering.rejection_cache_ttl_secs, 60);
    }

    #[test]
    fn test_standard_node_args_rejection_cache_defaults() {
        let args = CommandParser::<StandardNodeArgs>::parse_from(["reth"]).args;

        assert_eq!(
            args.metering.resource_metering.rejection_cache_max_capacity,
            REJECTION_CACHE_MAX_CAPACITY
        );
        assert_eq!(
            args.metering.resource_metering.rejection_cache_ttl_secs,
            REJECTION_CACHE_TTL.as_secs()
        );
    }

    #[test]
    fn inspector_opcode_names_skips_state_prefix_and_unparseable() {
        let names = inspector_opcode_names(
            ["SLOAD".to_string()],
            ["SSTORE", "STATE_NEW_STORAGE_SLOT", "NOT_AN_OPCODE", "sstore"],
        );
        assert_eq!(names, vec!["SLOAD".to_string(), "SSTORE".to_string()]);
    }

    #[test]
    fn runner_accepts_schedule_state_effect_operation_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "cpu",
                    "blockLimit": 1000000,
                    "operations": [
                        {"name": "SSTORE", "countCost": 1},
                        {"name": "STATE_NEW_STORAGE_SLOT", "countCost": 1},
                        {"name": "NOT_AN_OPCODE", "countCost": 1}
                    ]
                }]
            }"#,
        )
        .expect("write schedule");

        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-metering",
            "--payload.resource-metering-schedule",
            path.to_str().expect("utf-8 path"),
        ])
        .args;

        StandardBaseRethNode::runner(args)
            .expect("STATE_ and unknown schedule names must not fail opcode parse");
    }
}
