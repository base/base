//! Chainless execution-node arguments and launch helpers.

use std::{path::PathBuf, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use base_node_runner::LaunchedBaseNode;
use clap::{Args, value_parser};
use reth_cli_runner::CliContext;
use reth_db::init_db;
use reth_node_builder::NodeBuilder;
use reth_node_core::{
    args::{
        DatabaseArgs, DatadirArgs, DebugArgs, DevArgs, EngineArgs, EraArgs, MetricArgs,
        NetworkArgs, PruningArgs, RpcServerArgs, StaticFilesArgs, StorageArgs, TxPoolArgs,
    },
    node_config::NodeConfig,
    version,
};
use reth_rpc_server_types::{LenientRpcModuleValidator, RpcModuleValidator};
use tracing::info;

use crate::{RpcStandardNodeArgs, StandardNodeArgs};

/// Execution node arguments shared by binaries that provide chain selection themselves.
#[derive(Debug, Clone, Args)]
pub struct ExecutionNodeArgs {
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

    /// All ERA related arguments with --era prefix.
    #[command(flatten, next_help_heading = "ERA")]
    pub era: EraArgs,

    /// All static files related arguments with --static-files prefix.
    #[command(flatten, next_help_heading = "Static Files")]
    pub static_files: StaticFilesArgs,

    /// All storage related arguments with --storage prefix.
    #[command(flatten, next_help_heading = "Storage")]
    pub storage: StorageArgs,

    /// Standard Base execution-node extension arguments.
    #[command(flatten)]
    pub standard: RpcStandardNodeArgs,
}

impl ExecutionNodeArgs {
    /// Converts parsed args into a launchable execution node configuration.
    pub fn into_launch_config(self, chain: Arc<BaseChainSpec>) -> ExecutionNodeLaunchConfig {
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
            era,
            static_files,
            storage,
            standard,
        } = self;

        let node_config = NodeConfig {
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
            era,
            static_files,
            storage,
        };

        ExecutionNodeLaunchConfig { node_config, standard: standard.into(), with_unused_ports }
    }
}

/// A chain-injected execution node configuration ready to launch.
#[derive(Debug, Clone)]
pub struct ExecutionNodeLaunchConfig {
    /// Reth node configuration.
    pub node_config: NodeConfig<BaseChainSpec>,
    /// Standard Base execution-node extension arguments.
    pub standard: StandardNodeArgs,
    /// Whether all ports should be assigned by the OS.
    pub with_unused_ports: bool,
}

impl ExecutionNodeLaunchConfig {
    /// Enables authenticated Engine API over IPC.
    pub const fn with_auth_ipc(mut self) -> Self {
        self.node_config.rpc.auth_ipc = true;
        self
    }

    /// Returns the configured authenticated Engine API IPC path.
    pub const fn auth_ipc_path(&self) -> &str {
        self.node_config.rpc.auth_ipc_path.as_str()
    }

    /// Launches the execution node and returns its handle.
    pub async fn launch<Rpc>(mut self, ctx: CliContext) -> eyre::Result<LaunchedBaseNode>
    where
        Rpc: RpcModuleValidator,
    {
        if let Some(http_api) = &self.node_config.rpc.http_api {
            Rpc::validate_selection(http_api, "http.api").map_err(|e| eyre::eyre!("{e}"))?;
        }
        if let Some(ws_api) = &self.node_config.rpc.ws_api {
            Rpc::validate_selection(ws_api, "ws.api").map_err(|e| eyre::eyre!("{e}"))?;
        }

        info!(
            target: "reth::cli",
            version = ?version::version_metadata().short_version,
            client = %version::version_metadata().name_client,
            "Starting client"
        );

        if self.with_unused_ports {
            self.node_config = self.node_config.with_unused_ports();
        }

        let data_dir = self.node_config.datadir();
        let db_path = data_dir.db();
        info!(target: "reth::cli", path = ?db_path, "Opening database");
        let database =
            init_db(db_path.clone(), self.node_config.db.database_args())?.with_metrics();

        let builder = NodeBuilder::new(self.node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        crate::StandardBaseRethNode::launch(builder, self.standard).await
    }

    /// Launches the execution node with the default RPC module validator.
    pub async fn launch_default(self, ctx: CliContext) -> eyre::Result<LaunchedBaseNode> {
        self.launch::<LenientRpcModuleValidator>(ctx).await
    }
}
