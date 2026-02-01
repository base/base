//! In-process Base client node.
//!
//! Replaces Docker-based `ClientContainer` with an in-process node for faster tests.

use std::{any::Any, io::Write, net::SocketAddr, path::PathBuf, sync::Arc};

use base_client_node::{BaseBuilder, BaseNode, BaseNodeExtension};
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_txpool::{TxPoolExtension, TxpoolConfig};
use eyre::{Result, eyre};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db, mdbx::DatabaseArguments, test_utils::tempdir_path,
};
use reth_node_builder::{
    EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle, TreeConfig,
};
use reth_node_core::{
    args::{DatadirArgs, DiscoveryArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
    exit::NodeExitFuture,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::args::RollupArgs;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::TaskManager;
use url::Url;

/// Configuration for starting an in-process client node.
#[derive(Debug, Clone)]
pub struct InProcessClientConfig {
    /// L2 genesis JSON content.
    pub genesis_json: Vec<u8>,
    /// JWT secret hex for Engine API authentication.
    pub jwt_secret_hex: Vec<u8>,
    /// Builder HTTP RPC URL for rollup.sequencer.
    pub builder_rpc_url: String,
    /// Builder flashblocks WebSocket URL.
    pub builder_flashblocks_url: String,
    /// Builder P2P enode for trusted-peers.
    pub builder_p2p_enode: String,
}

/// In-process Base client node that syncs from a builder.
///
/// This replaces the Docker-based `ClientContainer` for faster test execution.
pub struct InProcessClient {
    http_api_addr: SocketAddr,
    ws_api_addr: SocketAddr,
    engine_addr: SocketAddr,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _task_manager: TaskManager,
    _db_path: PathBuf,
}

impl Drop for InProcessClient {
    fn drop(&mut self) {
        // Clean up the temporary database directory
        let _ = std::fs::remove_dir_all(&self._db_path);
    }
}

impl std::fmt::Debug for InProcessClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessClient")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .field("engine_addr", &self.engine_addr)
            .finish_non_exhaustive()
    }
}

impl InProcessClient {
    /// Starts an in-process client node with the provided configuration.
    pub async fn start(config: InProcessClientConfig) -> Result<Self> {
        let tasks = TaskManager::current();
        let exec = tasks.executor();

        // Parse genesis JSON to chain spec
        let genesis: alloy_genesis::Genesis = serde_json::from_slice(&config.genesis_json)
            .map_err(|e| eyre!("Failed to parse genesis JSON: {}", e))?;
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        // Network config: disable discovery, client syncs from builder only
        let network_config = NetworkArgs {
            discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
            trusted_peers: vec![config.builder_p2p_enode.parse()?],
            ..NetworkArgs::default()
        };

        let (db, db_path) = Self::create_test_database()?;
        let jwt_path = db_path.join("jwt.hex");
        let mut jwt_file = std::fs::File::create(&jwt_path)
            .map_err(|e| eyre!("Failed to create JWT file: {}", e))?;
        jwt_file
            .write_all(&config.jwt_secret_hex)
            .map_err(|e| eyre!("Failed to write JWT secret: {}", e))?;

        let unique_ipc_path = format!(
            "/tmp/reth_client_api_{}_{}_{:?}.ipc",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            std::process::id(),
            std::thread::current().id()
        );

        let mut rpc_args =
            RpcServerArgs::default().with_unused_ports().with_http().with_auth_ipc().with_ws();
        rpc_args.auth_ipc_path = unique_ipc_path;
        rpc_args.auth_jwtsecret = Some(jwt_path);

        // Configure rollup args with sequencer URL
        let rollup_args =
            RollupArgs { sequencer: Some(config.builder_rpc_url.clone()), ..Default::default() };

        let op_node = BaseNode::new(rollup_args.clone());

        let mut node_config = NodeConfig::new(Arc::clone(&chain_spec))
            .with_network(network_config)
            .with_rpc(rpc_args)
            .with_unused_ports();

        let datadir_path = MaybePlatformPath::<DataDirPath>::from(db_path.clone());
        node_config = node_config
            .with_datadir_args(DatadirArgs { datadir: datadir_path, ..Default::default() });

        let builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(exec.clone())
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(op_node.components())
            .with_add_ons(op_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        // Build extensions
        let extensions: Vec<Box<dyn BaseNodeExtension>> = Self::build_extensions(&config)?;

        // Apply all extensions
        let builder = extensions
            .into_iter()
            .fold(BaseBuilder::new(builder), |builder, extension| extension.apply(builder));

        // Launch with EngineNodeLauncher
        let NodeHandle { node: node_handle, node_exit_future } = builder
            .launch_with_fn(|builder| {
                let engine_tree_config = TreeConfig::default()
                    .with_persistence_threshold(builder.config().engine.persistence_threshold)
                    .with_memory_block_buffer_target(
                        builder.config().engine.memory_block_buffer_target,
                    );

                let launcher = EngineNodeLauncher::new(
                    builder.task_executor().clone(),
                    builder.config().datadir(),
                    engine_tree_config,
                );

                builder.launch_with(launcher)
            })
            .await?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre!("HTTP RPC server failed to bind to address"))?;

        let ws_api_addr = node_handle
            .rpc_server_handle()
            .ws_local_addr()
            .ok_or_else(|| eyre!("Failed to get websocket api address"))?;

        let engine_addr = node_handle.auth_server_handle().local_addr();

        Ok(Self {
            http_api_addr,
            ws_api_addr,
            engine_addr,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _task_manager: tasks,
            _db_path: db_path,
        })
    }

    /// Returns the HTTP RPC URL for the client.
    pub fn rpc_url(&self) -> Result<Url> {
        let url = Url::parse(&format!("http://{}", self.http_api_addr))
            .map_err(|e| eyre!("Failed to build HTTP URL: {}", e))?;
        Ok(url)
    }

    /// Returns the WebSocket URL for the client.
    pub fn ws_url(&self) -> Result<Url> {
        let url = Url::parse(&format!("ws://{}", self.ws_api_addr))
            .map_err(|e| eyre!("Failed to build WebSocket URL: {}", e))?;
        Ok(url)
    }

    /// Returns the Engine API URL using `host.docker.internal` for Docker containers.
    pub fn host_engine_url(&self) -> String {
        format!("http://host.docker.internal:{}", self.engine_addr.port())
    }

    /// Creates a test database with a 100 MB map size.
    fn create_test_database() -> Result<(Arc<DatabaseEnv>, PathBuf)> {
        let path = tempdir_path();
        let args = DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(100 * 1024 * 1024));
        let db = init_db(&path, args).expect("Failed to create test database");
        Ok((Arc::new(db), path))
    }

    /// Builds the extensions for the client node.
    fn build_extensions(config: &InProcessClientConfig) -> Result<Vec<Box<dyn BaseNodeExtension>>> {
        let mut extensions: Vec<Box<dyn BaseNodeExtension>> = Vec::new();

        // TxPool extension (tracing disabled for client)
        let flashblocks_url: Url = config
            .builder_flashblocks_url
            .parse()
            .map_err(|e| eyre!("Failed to parse flashblocks URL: {}", e))?;

        let flashblocks_config = FlashblocksConfig::new(flashblocks_url, 3);

        let txpool_config = TxpoolConfig {
            tracing_enabled: false,
            tracing_logs_enabled: false,
            sequencer_rpc: Some(config.builder_rpc_url.clone()),
            flashblocks_config: Some(flashblocks_config.clone()),
        };
        extensions.push(Box::new(TxPoolExtension::new(txpool_config)));

        // Flashblocks extension (must be last - uses replace_configured)
        extensions.push(Box::new(FlashblocksExtension::new(Some(flashblocks_config))));

        Ok(extensions)
    }
}
