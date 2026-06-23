//! Chainless execution-node arguments and launch helpers.

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeBuilder, LaunchedBaseNode};
use base_upgrade_signal::UpgradeSignalStartupMode;
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
use reth_rpc_server_types::{
    LenientRpcModuleValidator, RpcModuleValidator, constants::DEFAULT_ENGINE_API_IPC_ENDPOINT,
};
use tracing::info;

use crate::{RpcStandardNodeArgs, StandardNodeArgs};

const DEFAULT_BASE_MAX_INBOUND_EL_PEERS: usize = 80;
const DEFAULT_BASE_MAX_OUTBOUND_EL_PEERS: usize = 80;
const DEFAULT_UNIFIED_AUTH_IPC_FILENAME: &str = "engine.ipc";

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

    /// All ERA related arguments with --era prefix.
    #[command(flatten, next_help_heading = "ERA")]
    pub era: EraArgs,

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
            era,
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
            era,
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

    /// Standard Base execution-node extension arguments.
    #[command(flatten)]
    pub standard: RpcStandardNodeArgs,
}

impl ExecutionNodeArgs {
    /// Converts parsed args into a launchable standard execution node configuration.
    pub fn into_launch_config(self, chain: Arc<BaseChainSpec>) -> ExecutionNodeLaunchConfig {
        let runtime = self.node.into_runtime_config(chain);
        ExecutionNodeLaunchConfig {
            node_config: runtime.node_config,
            standard: self.standard.into(),
            with_unused_ports: runtime.with_unused_ports,
            upgrade_signal_startup: runtime.upgrade_signal_startup,
        }
    }
}

/// A chain-injected execution-node runtime configuration.
#[derive(Debug, Clone)]
pub struct ExecutionNodeRuntimeConfig {
    /// Reth node configuration.
    pub node_config: NodeConfig<BaseChainSpec>,
    /// Whether all ports should be assigned by the OS.
    pub with_unused_ports: bool,
    /// Whether this launch should perform its own upgrade-signal startup read.
    pub upgrade_signal_startup: UpgradeSignalStartupMode,
}

impl ExecutionNodeRuntimeConfig {
    /// Enables authenticated Engine API over IPC on the supplied node config.
    pub const fn enable_auth_ipc(node_config: &mut NodeConfig<BaseChainSpec>) {
        node_config.rpc.auth_ipc = true;
    }

    /// Configures the embedded execution node auth endpoint used by unified Base binaries.
    pub fn configure_unified_auth_endpoint(node_config: &mut NodeConfig<BaseChainSpec>) {
        let auth_ipc_path = if node_config.rpc.auth_ipc_path == DEFAULT_ENGINE_API_IPC_ENDPOINT {
            Some(node_config.datadir().data_dir().join(DEFAULT_UNIFIED_AUTH_IPC_FILENAME))
        } else {
            None
        };

        node_config.rpc.auth_ipc = true;
        node_config.rpc.auth_port = 0;
        node_config.rpc.auth_addr = Ipv4Addr::LOCALHOST.into();

        if let Some(auth_ipc_path) = auth_ipc_path {
            node_config.rpc.auth_ipc_path = auth_ipc_path.to_string_lossy().into_owned();
        }
    }

    /// Returns the configured authenticated Engine API IPC path from the supplied node config.
    pub const fn auth_ipc_path_for(node_config: &NodeConfig<BaseChainSpec>) -> &str {
        node_config.rpc.auth_ipc_path.as_str()
    }

    /// Enables authenticated Engine API over IPC.
    pub const fn with_auth_ipc(mut self) -> Self {
        Self::enable_auth_ipc(&mut self.node_config);
        self
    }

    /// Marks the upgrade-signal startup schedule as already applied by the caller.
    pub const fn with_upgrade_signal_startup_already_applied(mut self) -> Self {
        self.upgrade_signal_startup = UpgradeSignalStartupMode::AlreadyApplied;
        self
    }

    /// Configures authenticated Engine API access for unified Base binaries.
    pub fn with_unified_auth_endpoint(mut self) -> Self {
        Self::configure_unified_auth_endpoint(&mut self.node_config);
        self
    }

    /// Returns the configured authenticated Engine API IPC path.
    pub const fn auth_ipc_path(&self) -> &str {
        Self::auth_ipc_path_for(&self.node_config)
    }

    /// Converts the runtime config into a reth node builder.
    pub fn into_node_builder<Rpc>(mut self, ctx: CliContext) -> eyre::Result<BaseNodeBuilder>
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
        let database = init_db(db_path, self.node_config.db.database_args())?.with_metrics();

        let builder = NodeBuilder::new(self.node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        Ok(builder)
    }

    /// Converts the runtime config into a reth node builder with the default RPC validator.
    pub fn into_default_node_builder(self, ctx: CliContext) -> eyre::Result<BaseNodeBuilder> {
        self.into_node_builder::<LenientRpcModuleValidator>(ctx)
    }
}

/// A chain-injected standard execution node configuration ready to launch.
#[derive(Debug, Clone)]
pub struct ExecutionNodeLaunchConfig {
    /// Reth node configuration.
    pub node_config: NodeConfig<BaseChainSpec>,
    /// Standard Base execution-node extension arguments.
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

    /// Enables authenticated Engine API over IPC.
    pub const fn with_auth_ipc(mut self) -> Self {
        ExecutionNodeRuntimeConfig::enable_auth_ipc(&mut self.node_config);
        self
    }

    /// Marks the upgrade-signal startup schedule as already applied by the caller.
    pub const fn with_upgrade_signal_startup_already_applied(mut self) -> Self {
        self.upgrade_signal_startup = UpgradeSignalStartupMode::AlreadyApplied;
        self
    }

    /// Configures authenticated Engine API access for unified Base binaries.
    pub fn with_unified_auth_endpoint(mut self) -> Self {
        ExecutionNodeRuntimeConfig::configure_unified_auth_endpoint(&mut self.node_config);
        self
    }

    /// Returns the configured authenticated Engine API IPC path.
    pub const fn auth_ipc_path(&self) -> &str {
        ExecutionNodeRuntimeConfig::auth_ipc_path_for(&self.node_config)
    }

    /// Launches the execution node and returns its handle.
    pub async fn launch<Rpc>(self, ctx: CliContext) -> eyre::Result<LaunchedBaseNode>
    where
        Rpc: RpcModuleValidator,
    {
        let (execution, standard) = self.into_runtime_config();
        let upgrade_signal_startup = execution.upgrade_signal_startup;
        let builder = execution.into_node_builder::<Rpc>(ctx)?;
        crate::StandardBaseRethNode::launch_with_upgrade_signal_startup(
            builder,
            standard,
            upgrade_signal_startup,
        )
        .await
    }

    /// Launches the execution node with the default RPC module validator.
    pub async fn launch_default(self, ctx: CliContext) -> eyre::Result<LaunchedBaseNode> {
        self.launch::<LenientRpcModuleValidator>(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use url::Url;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn shared_execution_args_parse_without_standard_node_args() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from([
            "reth",
            "--port",
            "30333",
            "--auth-ipc.path=/tmp/engine.ipc",
        ])
        .args;

        assert_eq!(args.network.port, 30333);

        let runtime = args.into_runtime_config(Arc::new(BaseChainSpec::devnet()));
        assert_eq!(runtime.node_config.rpc.auth_ipc_path, "/tmp/engine.ipc");
        assert!(!runtime.node_config.rpc.auth_ipc);
    }

    #[test]
    fn standard_execution_args_keep_base_extension_args_separate() {
        let args = CommandParser::<ExecutionNodeArgs>::parse_from([
            "reth",
            "--port",
            "30333",
            "--flashblocks-url",
            "wss://example.com/ws",
        ])
        .args;

        assert_eq!(args.node.network.port, 30333);
        assert_eq!(
            args.standard.flashblocks_url.as_ref().map(Url::as_str),
            Some("wss://example.com/ws")
        );
    }

    #[test]
    fn runtime_config_can_enable_auth_ipc_without_standard_node_args() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from([
            "reth",
            "--auth-ipc.path=/tmp/engine.ipc",
        ])
        .args;

        let runtime = args.into_runtime_config(Arc::new(BaseChainSpec::devnet())).with_auth_ipc();

        assert!(runtime.node_config.rpc.auth_ipc);
        assert_eq!(runtime.auth_ipc_path(), "/tmp/engine.ipc");
    }

    #[test]
    fn runtime_config_uses_datadir_auth_ipc_path_for_unified_defaults() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from([
            "reth",
            "--datadir=/tmp/base-node-a",
        ])
        .args;

        let runtime = args
            .into_runtime_config(Arc::new(BaseChainSpec::devnet()))
            .with_unified_auth_endpoint();
        let expected = runtime
            .node_config
            .datadir()
            .data_dir()
            .join(DEFAULT_UNIFIED_AUTH_IPC_FILENAME)
            .to_string_lossy()
            .into_owned();

        assert!(runtime.node_config.rpc.auth_ipc);
        assert_eq!(runtime.node_config.rpc.auth_port, 0);
        assert_eq!(runtime.node_config.rpc.auth_addr, Ipv4Addr::LOCALHOST);
        assert_eq!(runtime.auth_ipc_path(), expected);
    }

    #[test]
    fn runtime_config_preserves_explicit_auth_ipc_path_for_unified() {
        let args = CommandParser::<ExecutionNodeConfigArgs>::parse_from([
            "reth",
            "--datadir=/tmp/base-node-a",
            "--auth-ipc.path=/tmp/custom-engine.ipc",
        ])
        .args;

        let runtime = args
            .into_runtime_config(Arc::new(BaseChainSpec::devnet()))
            .with_unified_auth_endpoint();

        assert!(runtime.node_config.rpc.auth_ipc);
        assert_eq!(runtime.node_config.rpc.auth_port, 0);
        assert_eq!(runtime.node_config.rpc.auth_addr, Ipv4Addr::LOCALHOST);
        assert_eq!(runtime.auth_ipc_path(), "/tmp/custom-engine.ipc");
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
    fn launch_config_delegates_auth_ipc_to_runtime_config() {
        let args = CommandParser::<ExecutionNodeArgs>::parse_from([
            "reth",
            "--auth-ipc.path=/tmp/engine.ipc",
        ])
        .args;

        let launch = args.into_launch_config(Arc::new(BaseChainSpec::devnet())).with_auth_ipc();

        assert!(launch.node_config.rpc.auth_ipc);
        assert_eq!(launch.auth_ipc_path(), "/tmp/engine.ipc");

        let (runtime, _standard) = launch.into_runtime_config();
        assert!(runtime.node_config.rpc.auth_ipc);
        assert_eq!(runtime.auth_ipc_path(), "/tmp/engine.ipc");
    }
}
