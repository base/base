//! Standard Base execution-node arguments and runner wiring.

use std::{sync::Arc, time::Duration};

use base_bundle_extension::BundleExtension;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_metering::{MeteredOpcodes, MeteringConfig, MeteringExtension, MeteringResourceLimits};
use base_node_core::args::RollupArgs;
use base_node_runner::{BaseNodeBuilder, BaseNodeRunner, LaunchedBaseNode, PayloadServiceBuilder};
use base_proofs_extension::ProofsHistoryExtension;
use base_tx_forwarding::{
    DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS, DEFAULT_RESEND_AFTER_MS, TxForwardingConfig,
    TxForwardingExtension,
};
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};
use base_upgrade_signal::UpgradeSignalStartupMode;
use url::Url;

use crate::upgrade_signal::{
    ExecutionUpgradeSignal, ExecutionUpgradeSignalConfig, ExecutionUpgradeSignalMetricsExtension,
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

    /// Per-flashblock execution time budget in microseconds for priority fee estimation.
    #[arg(long = "metering.execution-time-us", requires = "enable_metering")]
    pub metering_execution_time_us: Option<u64>,

    /// Whole-block state root computation budget in microseconds for priority fee estimation.
    #[arg(
        long = "metering.state-root-time-us",
        requires_all = ["enable_metering", "metering_target_flashblocks_per_block"]
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
    /// This excludes the base flashblock at index `0` and is required when gas, state root
    /// time, or DA estimation is enabled.
    #[arg(long = "metering.target-flashblocks-per-block", requires = "enable_metering")]
    pub metering_target_flashblocks_per_block: Option<usize>,

    /// Comma-separated list of EVM opcodes to track for gas metering
    /// (e.g., "SSTORE,SLOAD,KECCAK256"). Precompile gas is always tracked.
    #[arg(long = "metering.metered-opcodes", requires = "enable_metering", value_delimiter = ',')]
    pub metering_metered_opcodes: Vec<String>,
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

    /// Enable cached execution via the flashblocks-aware engine validator.
    #[arg(long = "flashblocks.cached-execution", requires = "flashblocks_url")]
    pub flashblocks_cached_execution: bool,

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
}

impl From<RpcStandardNodeArgs> for StandardNodeArgs {
    fn from(mut args: RpcStandardNodeArgs) -> Self {
        if args.rollup_args.sequencer.is_none() {
            args.rollup_args.sequencer.clone_from(&args.rpc_forwarding_endpoint);
        }

        Self {
            rpc: args,
            metering: MeteringArgs::default(),
            enable_tx_forwarding: false,
            builder_rpc_urls: Vec::new(),
            tx_forwarding_resend_after_ms: DEFAULT_RESEND_AFTER_MS,
            tx_forwarding_batch_size: DEFAULT_MAX_BATCH_SIZE,
            tx_forwarding_max_rps: DEFAULT_MAX_RPS,
        }
    }
}

impl StandardNodeArgs {
    /// Sets the metering arguments on this standard node configuration.
    pub fn with_metering(mut self, metering: MeteringArgs) -> Self {
        self.metering = metering;
        self
    }
}

impl From<&StandardNodeArgs> for Option<FlashblocksConfig> {
    fn from(args: &StandardNodeArgs) -> Self {
        args.rpc.flashblocks_url.clone().map(|url| {
            let mut config = FlashblocksConfig::new(url, args.rpc.max_pending_blocks_depth)
                .with_subscriber_ping_interval(args.rpc.flashblocks_ping_interval);
            config.cached_execution = args.rpc.flashblocks_cached_execution;
            config
        })
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
    /// Applies a configured L1 upgrade signal to the execution chain spec before startup.
    pub async fn apply_initial_upgrade_signal(
        builder: BaseNodeBuilder,
        args: &StandardNodeArgs,
    ) -> eyre::Result<BaseNodeBuilder> {
        Self::apply_initial_upgrade_signal_from_rollup_args(builder, &args.rpc.rollup_args).await
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

    /// Installs the upgrade signal metrics observer extension when configured.
    pub fn install_upgrade_signal_metrics_extension<SB: PayloadServiceBuilder>(
        runner: &mut BaseNodeRunner<SB>,
        rollup_args: &RollupArgs,
    ) -> eyre::Result<()> {
        let Some(config) = Self::upgrade_signal_config(rollup_args)? else {
            return Ok(());
        };

        runner.install_ext::<ExecutionUpgradeSignalMetricsExtension>(config);

        Ok(())
    }

    /// Validates execution upgrade signal arguments before node setup.
    ///
    /// Execution upgrade-signal polling is configured independently from consensus polling, so a
    /// configured contract always requires an explicit `--upgrade-signal.l1-rpc`. This holds for
    /// every mode, including the default metrics-only mode, which still polls the contract.
    pub fn validate_upgrade_signal_args(rollup_args: &RollupArgs) -> eyre::Result<()> {
        if rollup_args.upgrade_signal.contract_address.is_some()
            && rollup_args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.is_none()
        {
            eyre::bail!(
                "--upgrade-signal.contract (env BASE_NODE_UPGRADE_SIGNAL_CONTRACT) requires \
                 --upgrade-signal.l1-rpc (env BASE_NODE_UPGRADE_SIGNAL_L1_RPC) for execution \
                 upgrade-signal polling; every mode, including the default metrics-only mode, \
                 reads the contract over L1"
            );
        }

        Ok(())
    }

    fn upgrade_signal_config(
        rollup_args: &RollupArgs,
    ) -> eyre::Result<Option<ExecutionUpgradeSignalConfig>> {
        let Some(signal_config) = rollup_args.upgrade_signal.config()? else {
            return Ok(None);
        };
        Self::validate_upgrade_signal_args(rollup_args)?;
        let l1_rpc = rollup_args
            .upgrade_signal_l1_rpc
            .upgrade_signal_l1_rpc
            .clone()
            .expect("validated by validate_upgrade_signal_args");

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
            tracing_enabled: args.rpc.enable_transaction_tracing,
            tracing_logs_enabled: args.rpc.enable_transaction_tracing_logs,
            flashblocks_config: flashblocks_config.clone(),
        });

        let resource_limits = MeteringResourceLimits {
            gas_limit: args.metering.metering_gas_limit,
            execution_time_us: args.metering.metering_execution_time_us,
            state_root_time_us: args.metering.metering_state_root_time_us,
            da_bytes: args.metering.metering_da_bytes,
        };
        let metering_config = if args.metering.enable_metering {
            let metered_opcodes = if args.metering.metering_metered_opcodes.is_empty() {
                MeteredOpcodes::default()
            } else {
                MeteredOpcodes::parse(&args.metering.metering_metered_opcodes)?
            }
            .with_all_precompiles();

            let mut config = flashblocks_config
                .clone()
                .map_or_else(MeteringConfig::enabled, MeteringConfig::with_flashblocks)
                .with_resource_limits(resource_limits)
                .with_metered_opcodes(metered_opcodes);
            if let Some(target_flashblocks_per_block) =
                args.metering.metering_target_flashblocks_per_block
            {
                config = config.with_target_flashblocks_per_block(target_flashblocks_per_block);
            }
            config
        } else {
            MeteringConfig::disabled()
        };
        runner.install_ext::<MeteringExtension>(metering_config);
        runner.install_ext::<BundleExtension>(());
        runner.install_ext::<TxForwardingExtension>((&args).into());
        runner.install_ext::<ProofsHistoryExtension>(rollup_args.clone());
        Self::install_upgrade_signal_metrics_extension(&mut runner, &rollup_args)?;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
            flashblocks_cached_execution: false,
            flashblocks_ping_interval: Duration::from_secs(30),
            enable_transaction_tracing: false,
            enable_transaction_tracing_logs: false,
        }
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
        assert!(!config.enabled);
        assert!(config.builder_urls.is_empty());
    }

    #[test]
    fn test_upgrade_signal_contract_requires_execution_l1_rpc() {
        let error = StandardBaseRethNode::validate_upgrade_signal_args(&RollupArgs {
            upgrade_signal: base_upgrade_signal::UpgradeSignalArgs {
                contract_address: Some(address!("0000000000000000000000000000000000000001")),
                ..Default::default()
            },
            ..Default::default()
        })
        .expect_err("upgrade signal contract should require an explicit execution L1 RPC");

        assert!(error.to_string().contains("--upgrade-signal.l1-rpc"));
    }

    #[test]
    fn test_standard_node_args_parses_metering_flags_once() {
        let args = CommandParser::<StandardNodeArgs>::parse_from([
            "reth",
            "--enable-metering",
            "--metering.execution-time-us",
            "5000000",
        ])
        .args;

        assert!(args.metering.enable_metering);
        assert_eq!(args.metering.metering_execution_time_us, Some(5_000_000));
    }
}
