//! Standard Base execution-node arguments and runner wiring.

use std::{env, path::PathBuf, sync::Arc, time::Duration};

use base_bundle_extension::BundleExtension;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_metering::{MeteredOpcodes, MeteringConfig, MeteringExtension, MeteringResourceLimits};
use base_node_core::{HasRollupArgs, RollupArgs};
use base_node_runner::{BaseNodeBuilder, BaseNodeRunner, LaunchedBaseNode, PayloadServiceBuilder};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY,
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use base_proofs_extension::ProofsHistoryExtension;
use base_shadow_indexer::{ShadowIndexerConfig, ShadowIndexerExtension};
use base_shadow_indexer_db::ShadowDbConfig;
use base_tx_forwarding::{
    DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY, DEFAULT_INLINE_SIMULATION_TIMEOUT_MS,
    DEFAULT_INLINE_SIMULATION_WORKERS, DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS,
    DEFAULT_RESEND_AFTER_MS, TxForwardingConfig, TxForwardingExtension,
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

/// CLI arguments for metering RPC and priority-fee resource budgets.
#[derive(Debug, Clone, PartialEq, Eq, Default, clap::Args)]
pub struct MeteringArgs {
    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Whole-block gas budget for priority fee estimation.
    #[arg(
        long = "metering.gas-limit",
        requires_all = ["enable_metering", "metering_target_flashblocks_per_block"]
    )]
    pub metering_gas_limit: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(long = "metering.execution-time-us", requires = "enable_metering", hide = true)]
    pub metering_execution_time_us: Option<u64>,

    /// Deprecated and ignored. Kept so older deployment configurations remain accepted.
    #[arg(
        long = "metering.state-root-time-us",
        requires_all = ["enable_metering", "metering_target_flashblocks_per_block"],
        hide = true
    )]
    pub metering_state_root_time_us: Option<u64>,

    /// Whole-block data availability byte budget for priority fee estimation.
    #[arg(
        long = "metering.da-bytes",
        requires_all = ["enable_metering", "metering_target_flashblocks_per_block"]
    )]
    pub metering_da_bytes: Option<u64>,

    /// Target number of tx-pool flashblocks the builder budgets per block.
    ///
    /// This excludes the base flashblock at index `0` and is required when gas or DA
    /// estimation is enabled.
    #[arg(long = "metering.target-flashblocks-per-block", requires = "enable_metering")]
    pub metering_target_flashblocks_per_block: Option<usize>,

    /// Comma-separated list of EVM opcodes to track for gas metering
    /// (e.g., "SSTORE,SLOAD,KECCAK256"). Precompile gas is always tracked.
    #[arg(long = "metering.metered-opcodes", requires = "enable_metering", value_delimiter = ',')]
    pub metering_metered_opcodes: Vec<String>,
}

/// Default maximum number of open shadow indexer database connections.
const DEFAULT_SHADOW_INDEXER_MAX_CONNECTIONS: u32 = 5;
/// Default timeout when acquiring a shadow indexer database connection.
const DEFAULT_SHADOW_INDEXER_CONNECTION_TIMEOUT: &str = "30s";

/// CLI arguments for the shadow indexer `ExEx` that persists committed execution blocks.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ShadowIndexerArgs {
    /// Enable the shadow indexer `ExEx` that persists committed execution blocks to Postgres.
    #[arg(long = "enable-shadow-indexer", env = "ENABLE_SHADOW_INDEXER")]
    pub enable_shadow_indexer: bool,

    /// `PostgreSQL` connection URL for the shadow indexer database.
    #[arg(
        long = "shadow-indexer.database-url",
        env = "SHADOW_INDEXER_DATABASE_URL",
        value_name = "SHADOW_INDEXER_DATABASE_URL",
        requires = "enable_shadow_indexer"
    )]
    pub shadow_indexer_database_url: Option<String>,

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
}

impl Default for ShadowIndexerArgs {
    fn default() -> Self {
        Self {
            enable_shadow_indexer: false,
            shadow_indexer_database_url: None,
            shadow_indexer_max_connections: DEFAULT_SHADOW_INDEXER_MAX_CONNECTIONS,
            shadow_indexer_connection_timeout: humantime::parse_duration(
                DEFAULT_SHADOW_INDEXER_CONNECTION_TIMEOUT,
            )
            .expect("valid default shadow indexer connection timeout"),
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
    /// When transaction forwarding is enabled, validity predicates are forwarded to builders, but
    /// they are not yet enforced. This can also be enabled on a standalone sequencer (e.g. a local
    /// devnet) that builds blocks itself, in which case forwarding is not required.
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

    /// Run meter_bundle on the mempool node before inserting into the forwarding pool.
    ///
    /// Requires `--flashblocks-url` so sims use the live pending state, not an empty default.
    #[arg(
        long = "enable-inline-simulation",
        requires_all = ["enable_tx_forwarding", "flashblocks_url"]
    )]
    pub enable_inline_simulation: bool,

    /// Number of in-process meter_bundle workers.
    #[arg(
        long = "inline-simulation-workers",
        value_name = "INLINE_SIMULATION_WORKERS",
        default_value_t = DEFAULT_INLINE_SIMULATION_WORKERS,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..),
        requires = "enable_inline_simulation"
    )]
    pub inline_simulation_workers: usize,

    /// Bounded queue of txs waiting for meter_bundle.
    #[arg(
        long = "inline-simulation-queue-capacity",
        value_name = "INLINE_SIMULATION_QUEUE_CAPACITY",
        default_value_t = DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..),
        requires = "enable_inline_simulation"
    )]
    pub inline_simulation_queue_capacity: usize,

    /// Deadline for attaching real metering versus `MeterBundleResponse::default`.
    ///
    /// Does not free the worker or bound queue drain: `meter_bundle` still runs
    /// to completion, and the worker joins that blocking task.
    #[arg(
        long = "inline-simulation-timeout-ms",
        value_name = "INLINE_SIMULATION_TIMEOUT_MS",
        default_value_t = DEFAULT_INLINE_SIMULATION_TIMEOUT_MS,
        requires = "enable_inline_simulation"
    )]
    pub inline_simulation_timeout_ms: u64,
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
        let url = if args.enable_shadow_indexer {
            args.shadow_indexer_database_url.clone().ok_or_else(|| {
                eyre::eyre!(
                    "--enable-shadow-indexer (env ENABLE_SHADOW_INDEXER) requires \
                     --shadow-indexer.database-url (env SHADOW_INDEXER_DATABASE_URL)"
                )
            })?
        } else {
            String::new()
        };

        Ok(Self {
            enabled: args.enable_shadow_indexer,
            db: ShadowDbConfig {
                url,
                max_connections: args.shadow_indexer_max_connections,
                connection_timeout: args.shadow_indexer_connection_timeout,
            },
            builder_version: env!("CARGO_PKG_VERSION").to_string(),
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
            .with_inline_simulation(args.rpc.enable_inline_simulation)
            .with_inline_simulation_workers(args.rpc.inline_simulation_workers)
            .with_inline_simulation_queue_capacity(args.rpc.inline_simulation_queue_capacity)
            .with_inline_simulation_timeout_ms(args.rpc.inline_simulation_timeout_ms)
    }
}

/// Metering RPC config for a standard node.
///
/// RPC methods (`base_meterBundle`, `base_meterBlockByHash`, …) are registered
/// only when `--enable-metering` is set. Inline simulation uses the in-process
/// `meter_bundle` API and does not expose this surface.
fn metering_config(
    args: &StandardNodeArgs,
    flashblocks_config: Option<FlashblocksConfig>,
) -> eyre::Result<MeteringConfig> {
    if !args.metering.enable_metering {
        return Ok(MeteringConfig::disabled());
    }

    let resource_limits = MeteringResourceLimits {
        gas_limit: args.metering.metering_gas_limit,
        da_bytes: args.metering.metering_da_bytes,
    };
    let metered_opcodes = if args.metering.metering_metered_opcodes.is_empty() {
        MeteredOpcodes::default()
    } else {
        MeteredOpcodes::parse(&args.metering.metering_metered_opcodes)?
    }
    .with_all_precompiles();

    let mut config = flashblocks_config
        .map_or_else(MeteringConfig::enabled, MeteringConfig::with_flashblocks)
        .with_resource_limits(resource_limits)
        .with_metered_opcodes(metered_opcodes);
    if let Some(target_flashblocks_per_block) = args.metering.metering_target_flashblocks_per_block
    {
        config = config.with_target_flashblocks_per_block(target_flashblocks_per_block);
    }
    Ok(config)
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
        {
            warn!("deprecated metering resource limit flags are ignored");
        }

        runner.install_ext::<MeteringExtension>(metering_config(&args, flashblocks_config.clone())?);
        runner.install_ext::<ShadowIndexerExtension>((&args.shadow_indexer).try_into()?);
        runner.install_ext::<BundleExtension>(());
        let mut tx_forwarding_config: TxForwardingConfig = (&args).into();
        if tx_forwarding_config.inline_simulation {
            let Some(flashblocks) = flashblocks_config.as_ref() else {
                eyre::bail!("--enable-inline-simulation requires --flashblocks-url");
            };
            tx_forwarding_config =
                tx_forwarding_config.with_flashblocks_state(Some(Arc::clone(&flashblocks.state)));
        }
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
            enable_inline_simulation: false,
            inline_simulation_workers: DEFAULT_INLINE_SIMULATION_WORKERS,
            inline_simulation_queue_capacity: DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY,
            inline_simulation_timeout_ms: DEFAULT_INLINE_SIMULATION_TIMEOUT_MS,
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
        assert!(!config.inline_simulation);
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
    fn inline_simulation_requires_forwarding() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "base-reth",
            "--enable-inline-simulation",
        ])
        .expect_err("inline simulation should require forwarding");

        assert!(error.to_string().contains("--enable-tx-forwarding"));
    }

    #[test]
    fn inline_simulation_requires_flashblocks_url() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--enable-inline-simulation",
        ])
        .expect_err("inline simulation should require a flashblocks websocket");

        assert!(error.to_string().contains("--flashblocks-url"));
    }

    #[test]
    fn inline_simulation_workers_require_enable_flag() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--inline-simulation-workers",
            "8",
        ])
        .expect_err("worker count should require --enable-inline-simulation");

        assert!(error.to_string().contains("--enable-inline-simulation"));
    }

    #[test]
    fn inline_simulation_rejects_zero_workers() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--flashblocks-url",
            "wss://example.com/ws",
            "--enable-inline-simulation",
            "--inline-simulation-workers",
            "0",
        ])
        .expect_err("zero workers would install a queue with no consumers");

        assert!(error.to_string().contains("inline-simulation-workers"));
    }

    #[test]
    fn forwarding_without_inline_simulation_stays_off() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
        ])
        .args;
        let config = TxForwardingConfig::from(&args);

        assert!(config.enabled);
        assert!(!config.inline_simulation);
        assert_eq!(config.inline_simulation_workers, DEFAULT_INLINE_SIMULATION_WORKERS);
        assert_eq!(
            config.inline_simulation_queue_capacity,
            DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY
        );
        assert_eq!(config.inline_simulation_timeout_ms, DEFAULT_INLINE_SIMULATION_TIMEOUT_MS);
    }

    #[test]
    fn parses_inline_simulation_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--flashblocks-url",
            "wss://example.com/ws",
            "--enable-inline-simulation",
            "--inline-simulation-workers",
            "8",
            "--inline-simulation-queue-capacity",
            "32",
            "--inline-simulation-timeout-ms",
            "500",
        ])
        .args;
        let config = TxForwardingConfig::from(&args);

        assert!(args.rpc.enable_inline_simulation);
        assert_eq!(args.rpc.inline_simulation_workers, 8);
        assert_eq!(args.rpc.inline_simulation_queue_capacity, 32);
        assert_eq!(args.rpc.inline_simulation_timeout_ms, 500);
        assert!(config.inline_simulation);
        assert_eq!(config.inline_simulation_workers, 8);
        assert_eq!(config.inline_simulation_queue_capacity, 32);
        assert_eq!(config.inline_simulation_timeout_ms, 500);
    }

    #[test]
    fn inline_simulation_does_not_enable_metering_rpc() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--flashblocks-url",
            "wss://example.com/ws",
            "--enable-inline-simulation",
        ])
        .args;

        assert!(!args.metering.enable_metering);
        let config =
            metering_config(&args, None).expect("inline sim should not register metering RPC");
        assert!(!config.enabled);
    }

    #[test]
    fn enable_metering_registers_metering_rpc() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-metering",
        ])
        .args;

        let config = metering_config(&args, None).expect("enable-metering should register RPC");
        assert!(config.enabled);
    }

    #[test]
    fn forwarding_without_inline_simulation_leaves_metering_disabled() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "base-reth",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
        ])
        .args;

        let config = metering_config(&args, None).expect("forwarding-only config");
        assert!(!config.enabled);
    }

    #[test]
    fn programmatic_validity_config_without_forwarding_is_valid() {
        let mut args = StandardNodeArgs::from(default_rpc_standard_node_args());
        args.rpc.enable_experimental_validity_transactions = true;

        StandardBaseRethNode::runner(args)
            .expect("validity transactions should not require forwarding");
    }

    #[test]
    fn runner_rejects_inline_simulation_without_flashblocks() {
        let mut args = StandardNodeArgs::from(default_rpc_standard_node_args());
        args.rpc.enable_tx_forwarding = true;
        args.rpc.builder_rpc_urls = vec!["http://localhost:8545".parse().unwrap()];
        args.rpc.enable_inline_simulation = true;

        let error = StandardBaseRethNode::runner(args)
            .expect_err("inline simulation must not start without flashblocks state");

        assert!(error.to_string().contains("--flashblocks-url"));
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
            "--metering.target-flashblocks-per-block",
            "4",
            "--metering.gas-limit",
            "30000000",
        ])
        .args;

        assert!(args.metering.enable_metering);
        assert_eq!(args.metering.metering_gas_limit, Some(30_000_000));
    }

    #[test]
    fn test_standard_node_args_parses_shadow_indexer_flags() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-shadow-indexer",
            "--shadow-indexer.database-url",
            "postgres://localhost/shadow",
            "--shadow-indexer.max-connections",
            "9",
            "--shadow-indexer.connection-timeout",
            "45s",
        ])
        .args;

        assert!(args.shadow_indexer.enable_shadow_indexer);
        assert_eq!(
            args.shadow_indexer.shadow_indexer_database_url.as_deref(),
            Some("postgres://localhost/shadow")
        );
        assert_eq!(args.shadow_indexer.shadow_indexer_max_connections, 9);
        assert_eq!(args.shadow_indexer.shadow_indexer_connection_timeout, Duration::from_secs(45));
    }

    #[test]
    fn test_shadow_indexer_database_url_requires_enable_flag() {
        let error = CommandParser::<StandardNodeArgs>::try_parse_from([
            "reth",
            "--shadow-indexer.database-url",
            "postgres://localhost/shadow",
        ])
        .expect_err("shadow indexer database url should require the enable flag");

        assert!(error.to_string().contains("--enable-shadow-indexer"));
    }

    #[test]
    fn test_shadow_indexer_config_requires_database_url_when_enabled() {
        let args =
            ShadowIndexerArgs { enable_shadow_indexer: true, ..ShadowIndexerArgs::default() };
        let error = ShadowIndexerConfig::try_from(&args)
            .expect_err("enabled shadow indexer should require a database url");

        assert!(error.to_string().contains("--shadow-indexer.database-url"));
    }

    #[test]
    fn test_shadow_indexer_config_disabled_by_default() {
        let config = ShadowIndexerConfig::try_from(&ShadowIndexerArgs::default())
            .expect("disabled shadow indexer config should build without a url");

        assert!(!config.enabled);
        assert_eq!(config.builder_version, env!("CARGO_PKG_VERSION"));
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
            "--metering.target-flashblocks-per-block",
            "4",
        ])
        .args;

        assert_eq!(args.metering.metering_execution_time_us, Some(5_000_000));
        assert_eq!(args.metering.metering_state_root_time_us, Some(1_000_000));
    }
}
