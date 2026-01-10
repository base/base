//! Local node setup with Base Sepolia chainspec

use std::{any::Any, fmt, net::SocketAddr, sync::Arc};

use alloy_genesis::Genesis;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use eyre::Result;
use futures_util::Future;
use op_alloy_network::Optimism;
use reth::{
    api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter},
    args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
    builder::{
        Node, NodeBuilder, NodeBuilderWithComponents, NodeConfig, NodeHandle, WithLaunchContext,
    },
    core::exit::NodeExitFuture,
    tasks::TaskManager,
};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::DatabaseArguments,
    test_utils::{ERROR_DB_CREATION, TempDatabase, tempdir_path},
};
use reth_e2e_test_utils::{Adapter, TmpDB};
use reth_node_core::{
    args::DatadirArgs,
    dirs::{DataDirPath, MaybePlatformPath},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{OpNode, args::RollupArgs};
use reth_provider::providers::BlockchainProvider;

use crate::engine::EngineApi;

/// Convenience alias for the local blockchain provider type.
pub type LocalNodeProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, TmpDB>>;

/// Handle to a launched local node along with the resources required to keep it alive.
pub struct LocalNode {
    pub(crate) http_api_addr: SocketAddr,
    engine_ipc_path: String,
    pub(crate) ws_api_addr: SocketAddr,
    provider: LocalNodeProvider,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _task_manager: TaskManager,
}

impl fmt::Debug for LocalNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalNode")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .field("engine_ipc_path", &self.engine_ipc_path)
            .finish_non_exhaustive()
    }
}

/// Optimism node types used for the local harness.
pub type OpTypes =
    FullNodeTypesAdapter<OpNode, TmpDB, BlockchainProvider<NodeTypesWithDBAdapter<OpNode, TmpDB>>>;
/// Builder that wires up the concrete node components.
pub type OpComponentsBuilder = <OpNode as Node<OpTypes>>::ComponentsBuilder;
/// Additional services attached to the node builder.
pub type OpAddOns = <OpNode as Node<OpTypes>>::AddOns;
/// Launcher builder used by the harness to customize node startup.
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpTypes, OpComponentsBuilder, OpAddOns>>;

/// Default launcher that is reused across the harness and integration tests.
pub async fn default_launcher(
    builder: OpBuilder,
) -> eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>> {
    let launcher = builder.engine_api_launcher();
    builder.launch_with(launcher).await
}

impl LocalNode {
    /// Launch a new local node using the provided launcher function.
    pub async fn new<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        build_node(launcher).await
    }

    /// Creates a test database with a smaller map size to reduce memory usage.
    ///
    /// Unlike `NodeBuilder::testing_node()` which hardcodes an 8 TB map size,
    /// this method configures the database with a 100 MB map size. This prevents
    /// `ENOMEM` errors when running parallel tests with `cargo test`, as the
    /// default 8 TB size can cause memory exhaustion when multiple test processes
    /// run concurrently.
    fn create_test_database() -> Result<Arc<TempDatabase<DatabaseEnv>>> {
        let default_size = 100 * 1024 * 1024; // 100 MB
        Self::create_test_database_with_size(default_size)
    }

    /// Creates a test database with a configurable map size to reduce memory usage.
    ///
    /// # Arguments
    ///
    /// * `max_size` - Maximum map size in bytes.
    fn create_test_database_with_size(max_size: usize) -> Result<Arc<TempDatabase<DatabaseEnv>>> {
        let path = tempdir_path();
        let emsg = format!("{ERROR_DB_CREATION}: {path:?}");
        let args =
            DatabaseArguments::new(ClientVersion::default()).with_geometry_max_size(Some(max_size));
        let db = init_db(&path, args).expect(&emsg);
        Ok(Arc::new(TempDatabase::new(db, path)))
    }

    /// Create an HTTP provider pointed at the node's public RPC endpoint.
    pub fn provider(&self) -> Result<RootProvider<Optimism>> {
        let url = format!("http://{}", self.http_api_addr);
        let client = RpcClient::builder().http(url.parse()?);
        Ok(RootProvider::<Optimism>::new(client))
    }

    /// Build an Engine API client that talks to the node's IPC endpoint.
    pub fn engine_api(&self) -> Result<EngineApi<crate::engine::IpcEngine>> {
        EngineApi::<crate::engine::IpcEngine>::new(self.engine_ipc_path.clone())
    }

    /// Clone the underlying blockchain provider so callers can inspect chain state.
    pub fn blockchain_provider(&self) -> LocalNodeProvider {
        self.provider.clone()
    }

    /// Websocket URL for the local node.
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.ws_api_addr)
    }
}

async fn build_node<L, LRet>(launcher: L) -> Result<LocalNode>
where
    L: FnOnce(OpBuilder) -> LRet,
    LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
{
    let tasks = TaskManager::current();
    let exec = tasks.executor();

    let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))?;
    let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

    let network_config = NetworkArgs {
        discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
        ..NetworkArgs::default()
    };

    let unique_ipc_path = format!(
        "/tmp/reth_engine_api_{}_{}_{:?}.ipc",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        std::process::id(),
        std::thread::current().id()
    );

    let mut rpc_args =
        RpcServerArgs::default().with_unused_ports().with_http().with_auth_ipc().with_ws();
    rpc_args.auth_ipc_path = unique_ipc_path;

    let node = OpNode::new(RollupArgs::default());

    let temp_db = LocalNode::create_test_database()?;
    let db_path = temp_db.path().to_path_buf();

    let mut node_config = NodeConfig::new(chain_spec.clone())
        .with_network(network_config)
        .with_rpc(rpc_args)
        .with_unused_ports();

    let datadir_path = MaybePlatformPath::<DataDirPath>::from(db_path.clone());
    node_config =
        node_config.with_datadir_args(DatadirArgs { datadir: datadir_path, ..Default::default() });

    let builder = NodeBuilder::new(node_config.clone())
        .with_database(temp_db)
        .with_launch_context(exec.clone())
        .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
        .with_components(node.components_builder())
        .with_add_ons(node.add_ons());

    let NodeHandle { node: node_handle, node_exit_future } =
        builder.launch_with_fn(launcher).await?;

    let http_api_addr = node_handle
        .rpc_server_handle()
        .http_local_addr()
        .ok_or_else(|| eyre::eyre!("HTTP RPC server failed to bind to address"))?;

    let ws_api_addr = node_handle
        .rpc_server_handle()
        .ws_local_addr()
        .ok_or_else(|| eyre::eyre!("Failed to get websocket api address"))?;

    let engine_ipc_path = node_config.rpc.auth_ipc_path;
    let provider = node_handle.provider().clone();

    Ok(LocalNode {
        http_api_addr,
        ws_api_addr,
        engine_ipc_path,
        provider,
        _node_exit_future: node_exit_future,
        _node: Box::new(node_handle),
        _task_manager: tasks,
    })
}
