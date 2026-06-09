//! Standard Base execution-node arguments and runner wiring.

use base_bundle_extension::BundleExtension;
use base_metering::{MeteredOpcodes, MeteringConfig, MeteringExtension, MeteringResourceLimits};
use base_node_core::args::RollupArgs;
use base_node_runner::{BaseNodeBuilder, BaseNodeRunner, LaunchedBaseNode};
use base_proofs_extension::ProofsHistoryExtension;
use base_tx_forwarding::{
    DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS, DEFAULT_RESEND_AFTER_MS, TxForwardingConfig,
    TxForwardingExtension,
};
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};
use url::Url;

/// CLI arguments for a standard Base execution node.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct StandardNodeArgs {
    /// Shared execution node arguments.
    #[command(flatten)]
    pub rpc: RpcStandardNodeArgs,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Whole-block gas budget for priority fee estimation.
    #[arg(long = "metering.gas-limit", requires = "enable_metering")]
    pub metering_gas_limit: Option<u64>,

    /// Whole-block execution time budget in microseconds for priority fee estimation.
    #[arg(long = "metering.execution-time-us", requires = "enable_metering")]
    pub metering_execution_time_us: Option<u64>,

    /// Whole-block state root computation budget in microseconds for priority fee estimation.
    #[arg(long = "metering.state-root-time-us", requires = "enable_metering")]
    pub metering_state_root_time_us: Option<u64>,

    /// Whole-block data availability byte budget for priority fee estimation.
    #[arg(long = "metering.da-bytes", requires = "enable_metering")]
    pub metering_da_bytes: Option<u64>,

    /// Comma-separated list of EVM opcodes to track for gas metering
    /// (e.g., "SSTORE,SLOAD,KECCAK256"). Precompile gas is always tracked.
    #[arg(long = "metering.metered-opcodes", requires = "enable_metering", value_delimiter = ',')]
    pub metering_metered_opcodes: Vec<String>,

    /// Enable transaction forwarding for mempool nodes to builder RPC endpoints
    #[arg(
        long = "enable-tx-forwarding",
        value_name = "ENABLE_TX_FORWARDING",
        requires = "builder_rpc_urls"
    )]
    pub enable_tx_forwarding: bool,

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

/// CLI arguments for a Base execution node embedded by the unified RPC command.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct RpcStandardNodeArgs {
    /// Rollup arguments.
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// Enable transaction tracing for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,
}

impl From<RpcStandardNodeArgs> for StandardNodeArgs {
    fn from(args: RpcStandardNodeArgs) -> Self {
        Self {
            rpc: args,
            enable_metering: false,
            metering_gas_limit: None,
            metering_execution_time_us: None,
            metering_state_root_time_us: None,
            metering_da_bytes: None,
            metering_metered_opcodes: Vec::new(),
            enable_tx_forwarding: false,
            builder_rpc_urls: Vec::new(),
            tx_forwarding_resend_after_ms: DEFAULT_RESEND_AFTER_MS,
            tx_forwarding_batch_size: DEFAULT_MAX_BATCH_SIZE,
            tx_forwarding_max_rps: DEFAULT_MAX_RPS,
        }
    }
}

impl From<&StandardNodeArgs> for TxForwardingConfig {
    fn from(args: &StandardNodeArgs) -> Self {
        if !args.enable_tx_forwarding || args.builder_rpc_urls.is_empty() {
            return Self::default();
        }

        Self::new(args.builder_rpc_urls.clone())
            .with_resend_after_ms(args.tx_forwarding_resend_after_ms)
            .with_max_batch_size(args.tx_forwarding_batch_size)
            .with_max_rps(args.tx_forwarding_max_rps)
    }
}

/// Standard Base execution-node runner wiring.
#[derive(Debug, Clone, Copy)]
pub struct StandardBaseRethNode;

impl StandardBaseRethNode {
    /// Builds a runner with the standard Base execution-node extensions installed.
    pub fn runner(args: StandardNodeArgs) -> eyre::Result<BaseNodeRunner> {
        let mut runner = BaseNodeRunner::new(args.rpc.rollup_args.clone());

        // Feature extensions.
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig {
            sequencer_rpc: args.rpc.rollup_args.sequencer.clone(),
        });
        runner.install_ext::<TxPoolExtension>(TxpoolConfig {
            tracing_enabled: args.rpc.enable_transaction_tracing,
            tracing_logs_enabled: args.rpc.enable_transaction_tracing_logs,
        });

        let resource_limits = MeteringResourceLimits {
            gas_limit: args.metering_gas_limit,
            execution_time_us: args.metering_execution_time_us,
            state_root_time_us: args.metering_state_root_time_us,
            da_bytes: args.metering_da_bytes,
        };
        let metering_config = if args.enable_metering {
            let metered_opcodes = if args.metering_metered_opcodes.is_empty() {
                MeteredOpcodes::default()
            } else {
                MeteredOpcodes::parse(&args.metering_metered_opcodes)?
            }
            .with_all_precompiles();

            MeteringConfig::enabled()
                .with_resource_limits(resource_limits)
                .with_metered_opcodes(metered_opcodes)
        } else {
            MeteringConfig::disabled()
        };
        runner.install_ext::<MeteringExtension>(metering_config);
        runner.install_ext::<BundleExtension>(());
        runner.install_ext::<TxForwardingExtension>((&args).into());
        runner.install_ext::<ProofsHistoryExtension>(args.rpc.rollup_args);

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
        Self::runner_with_version_metrics(args)?.run(builder).await
    }

    /// Launches the node and returns immediately with a handle.
    pub async fn launch(
        builder: BaseNodeBuilder,
        args: StandardNodeArgs,
    ) -> eyre::Result<LaunchedBaseNode> {
        Self::runner_with_version_metrics(args)?.launch(builder).await
    }
}
