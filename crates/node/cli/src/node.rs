//! Chainless execution-node arguments and launch helpers.

use std::{path::PathBuf, sync::Arc};

use base_common_chain_activation::UpgradeSignalStartupMode;
use base_common_chain_config::BaseChainSpec;
use base_common_cli::CliContext;
use base_execution_state_database::init_db;
use base_node_config::{
    DatabaseArgs, DatadirArgs, DebugArgs, DevArgs, EngineArgs, MetricArgs, NetworkArgs, NodeConfig,
    PruningArgs, RpcServerArgs, StaticFilesArgs, StorageArgs, TxPoolArgs,
};
use base_node_service::{NodeHandle, NodeLaunch};
use clap::{Args, value_parser};
use tracing::info;

use crate::{MeteringArgs, RpcStandardNodeArgs, ShadowIndexerArgs, StandardNodeArgs};

const DEFAULT_BASE_MAX_INBOUND_EL_PEERS: usize = 80;
const DEFAULT_BASE_MAX_OUTBOUND_EL_PEERS: usize = 80;

/// Chainless execution-node arguments shared by embedded Base commands.
#[derive(Debug, Clone, Args)]
pub struct ExecutionNodeConfigArgs {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    pub config: Option<PathBuf>,

    /// Prometheus metrics configuration.
    #[command(flatten)]
    pub metrics: MetricArgs,

    /// Add a new instance of a node.
    ///
    /// Configures the ports of the node to avoid conflicts with the defaults.
    ///
    /// Max number of instances is 200.
    #[arg(long, value_name = "INSTANCE", global = true, value_parser = value_parser!(u16).range(1..=200))]
    pub instance: Option<u16>,

    /// Sets all ports to unused, allowing the OS to choose random unused ports when sockets are
    /// bound.
    #[arg(long, conflicts_with = "instance", global = true)]
    pub with_unused_ports: bool,

    /// All datadir related arguments.
    #[command(flatten)]
    pub datadir: DatadirArgs,

    /// All networking related arguments.
    #[command(flatten)]
    pub network: NetworkArgs,

    /// All rpc related arguments.
    #[command(flatten)]
    pub rpc: RpcServerArgs,

    /// All txpool related arguments with --txpool prefix.
    #[command(flatten)]
    pub txpool: TxPoolArgs,

    /// All debug related arguments with --debug prefix.
    #[command(flatten)]
    pub debug: DebugArgs,

    /// All database related arguments.
    #[command(flatten)]
    pub db: DatabaseArgs,

    /// All dev related arguments with --dev prefix.
    #[command(flatten)]
    pub dev: DevArgs,

    /// All pruning related arguments.
    #[command(flatten)]
    pub pruning: PruningArgs,

    /// Engine cli arguments.
    #[command(flatten, next_help_heading = "Engine")]
    pub engine: EngineArgs,

    /// All static files related arguments with --static-files prefix.
    #[command(flatten, next_help_heading = "Static Files")]
    pub static_files: StaticFilesArgs,

    /// All storage related arguments with --storage prefix.
    #[command(flatten, next_help_heading = "Storage")]
    pub storage: StorageArgs,
}

impl ExecutionNodeConfigArgs {
    /// Converts parsed args into a chain-injected execution runtime config.
    pub fn into_runtime_config(self, chain: Arc<BaseChainSpec>) -> ExecutionNodeRuntimeConfig {
        let Self {
            config,
            metrics,
            instance,
            with_unused_ports,
            datadir,
            network,
            rpc,
            txpool,
            debug,
            db,
            dev,
            pruning,
            engine,
            static_files,
            storage,
        } = self;

        let mut node_config = NodeConfig {
            datadir,
            config,
            chain,
            metrics,
            instance,
            network,
            rpc,
            txpool,
            builder: Default::default(),
            debug,
            db,
            dev,
            pruning,
            engine,
            static_files,
            storage,
        };

        if node_config.network.max_inbound_peers.is_none() {
            node_config.network.max_inbound_peers = Some(DEFAULT_BASE_MAX_INBOUND_EL_PEERS);
        }

        if node_config.network.max_outbound_peers.is_none() {
            node_config.network.max_outbound_peers = Some(DEFAULT_BASE_MAX_OUTBOUND_EL_PEERS);
        }

        ExecutionNodeRuntimeConfig {
            node_config,
            with_unused_ports,
            upgrade_signal_startup: UpgradeSignalStartupMode::ReadAndApply,
        }
    }
}

/// Execution node arguments shared by RPC-style binaries that provide chain selection themselves.
#[derive(Debug, Clone, Args)]
pub struct ExecutionNodeArgs {
    /// Shared execution node arguments.
    #[command(flatten)]
    pub node: ExecutionNodeConfigArgs,

    /// Standard Base execution-node service arguments.
    #[command(flatten)]
    pub standard: RpcStandardNodeArgs,

    /// Metering RPC and priority-fee resource budget arguments.
    #[command(flatten)]
    pub metering: MeteringArgs,

    /// Shadow indexer `ExEx` arguments.
    #[command(flatten)]
    pub shadow_indexer: ShadowIndexerArgs,
}

impl ExecutionNodeArgs {
    /// Converts parsed args into a launchable standard execution node configuration.
    pub fn into_launch_config(self, chain: Arc<BaseChainSpec>) -> ExecutionNodeLaunchConfig {
        let runtime = self.node.into_runtime_config(chain);
        ExecutionNodeLaunchConfig {
            node_config: runtime.node_config,
            standard: StandardNodeArgs::from(self.standard)
                .with_metering(self.metering)
                .with_shadow_indexer(self.shadow_indexer),
            with_unused_ports: runtime.with_unused_ports,
            upgrade_signal_startup: runtime.upgrade_signal_startup,
        }
    }
}

/// A chain-injected execution-node runtime configuration.
#[derive(Debug, Clone)]
pub struct ExecutionNodeRuntimeConfig {
    /// Reth node configuration.
    pub node_config: NodeConfig,
    /// Whether all ports should be assigned by the OS.
    pub with_unused_ports: bool,
    /// Whether this launch should perform its own upgrade-signal startup read.
    pub upgrade_signal_startup: UpgradeSignalStartupMode,
}

impl ExecutionNodeRuntimeConfig {
    /// Marks the upgrade-signal startup schedule as already applied by the caller.
    pub const fn with_upgrade_signal_startup_already_applied(mut self) -> Self {
        self.upgrade_signal_startup = UpgradeSignalStartupMode::AlreadyApplied;
        self
    }

    /// Opens the database and resolves the Base launch resources.
    pub fn into_launch(mut self, ctx: CliContext) -> eyre::Result<NodeLaunch> {
        info!(
            target: "reth::cli",
            version = ?base_node_config::version_metadata().short_version,
            client = %base_node_config::version_metadata().name_client,
            "Starting client"
        );

        if self.with_unused_ports {
            self.node_config = self.node_config.with_unused_ports();
        }

        let data_dir = self.node_config.datadir();
        let db_path = data_dir.db();
        info!(target: "reth::cli", path = ?db_path, "Opening database");
        let database = init_db(db_path, self.node_config.db.database_args())?.with_metrics();

        let builder = NodeLaunch::new(self.node_config, database, ctx.task_executor);

        Ok(builder)
    }
}

/// A chain-injected standard execution node configuration ready to launch.
#[derive(Debug, Clone)]
pub struct ExecutionNodeLaunchConfig {
    /// Reth node configuration.
    pub node_config: NodeConfig,
    /// Standard Base execution-node service arguments.
    pub standard: StandardNodeArgs,
    /// Whether all ports should be assigned by the OS.
    pub with_unused_ports: bool,
    /// Whether this launch should perform its own upgrade-signal startup read.
    pub upgrade_signal_startup: UpgradeSignalStartupMode,
}

impl ExecutionNodeLaunchConfig {
    /// Converts this standard launch config into the shared runtime config plus standard args.
    pub fn into_runtime_config(self) -> (ExecutionNodeRuntimeConfig, StandardNodeArgs) {
        let Self { node_config, standard, with_unused_ports, upgrade_signal_startup } = self;
        (
            ExecutionNodeRuntimeConfig { node_config, with_unused_ports, upgrade_signal_startup },
            standard,
        )
    }

    /// Marks the upgrade-signal startup schedule as already applied by the caller.
    pub const fn with_upgrade_signal_startup_already_applied(mut self) -> Self {
        self.upgrade_signal_startup = UpgradeSignalStartupMode::AlreadyApplied;
        self
    }

    /// Launches the execution node and returns its handle.
    pub async fn launch(self, ctx: CliContext) -> eyre::Result<NodeHandle> {
        let (execution, standard) = self.into_runtime_config();
        let upgrade_signal_startup = execution.upgrade_signal_startup;
        let builder = execution.into_launch(ctx)?;
        crate::StandardBaseRethNode::launch_with_upgrade_signal_startup(
            builder,
            standard,
            upgrade_signal_startup,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn execution_args_reject_era_import() {
        for arg in ["--era.enable", "--era.path=/tmp/era", "--era.url=https://example.com"] {
            let error = CommandParser::<ExecutionNodeConfigArgs>::try_parse_from(["base", arg])
                .expect_err("ERA import is no longer supported");
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn shared_execution_args_parse_without_standard_node_args() {
        let args =
            CommandParser::<ExecutionNodeConfigArgs>::parse_from(["reth", "--port", "30333"]).args;

        assert_eq!(args.network.port, 30333);

        let runtime = args.into_runtime_config(Arc::new(BaseChainSpec::devnet()));

        assert_eq!(runtime.node_config.network.port, 30333);
    }

    #[test]
    fn standard_execution_args_keep_base_extension_args_separate() {
        let args = CommandParser::<ExecutionNodeArgs>::parse_from(["reth", "--port", "30333"]).args;

        assert_eq!(args.node.network.port, 30333);

        assert!(!args.metering.enable_metering);
    }

    #[test]
    fn runtime_config_sets_base_default_el_peer_limits() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from(["reth"]).args;

        let runtime = args.into_runtime_config(Arc::new(BaseChainSpec::devnet()));

        assert_eq!(runtime.node_config.network.max_inbound_peers, Some(80));
        assert_eq!(runtime.node_config.network.max_outbound_peers, Some(80));
    }

    #[test]
    fn runtime_config_preserves_explicit_el_peer_limits() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from([
            "reth",
            "--max-inbound-peers",
            "12",
            "--max-outbound-peers",
            "34",
        ])
        .args;

        let runtime = args.into_runtime_config(Arc::new(BaseChainSpec::devnet()));

        assert_eq!(runtime.node_config.network.max_inbound_peers, Some(12));
        assert_eq!(runtime.node_config.network.max_outbound_peers, Some(34));
    }

    #[test]
    fn standard_execution_args_parse_metering_separately() {
        let args =
            CommandParser::<ExecutionNodeArgs>::parse_from(["reth", "--enable-metering"]).args;

        assert!(args.metering.enable_metering);

        let launch_config = args.into_launch_config(Arc::new(BaseChainSpec::devnet()));
        assert!(launch_config.standard.metering.enable_metering);
    }
}
