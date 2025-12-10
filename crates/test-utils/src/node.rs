//! Local node setup with Base Sepolia chainspec

use std::{
    any::Any,
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use alloy_genesis::Genesis;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use base_reth_flashblocks::{Flashblock, FlashblocksReceiver, FlashblocksState};
use base_reth_rpc::{EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer};
use eyre::Result;
use futures_util::Future;
use once_cell::sync::OnceCell;
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
use reth_exex::ExExEvent;
use reth_node_core::{
    args::DatadirArgs,
    dirs::{DataDirPath, MaybePlatformPath},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{OpNode, args::RollupArgs};
use reth_provider::{CanonStateSubscriptions, providers::BlockchainProvider};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;

use crate::engine::EngineApi;

/// Chain ID for the Base Sepolia environment spun up by the harness.
pub const BASE_CHAIN_ID: u64 = 84532;

/// Convenience alias for the local blockchain provider type.
pub type LocalNodeProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, TmpDB>>;
/// Convenience alias for the Flashblocks state backing the local node.
pub type LocalFlashblocksState = FlashblocksState<LocalNodeProvider>;

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

/// Components that allow tests to interact with the Flashblocks worker tasks.
#[derive(Clone)]
pub struct FlashblocksParts {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    state: Arc<LocalFlashblocksState>,
}

impl fmt::Debug for FlashblocksParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksParts").finish_non_exhaustive()
    }
}

impl FlashblocksParts {
    /// Clone the shared [`FlashblocksState`] handle.
    pub fn state(&self) -> Arc<LocalFlashblocksState> {
        self.state.clone()
    }

    /// Send a flashblock to the background processor and wait until it is handled.
    pub async fn send(&self, flashblock: Flashblock) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((flashblock, tx)).await.map_err(|err| eyre::eyre!(err))?;
        rx.await.map_err(|err| eyre::eyre!(err))?;
        Ok(())
    }
}

#[derive(Clone)]
struct FlashblocksNodeExtensions {
    inner: Arc<FlashblocksNodeExtensionsInner>,
}

struct FlashblocksNodeExtensionsInner {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    #[allow(clippy::type_complexity)]
    receiver: Arc<Mutex<Option<mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>>>>,
    fb_cell: Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    process_canonical: bool,
}

impl FlashblocksNodeExtensions {
    fn new(process_canonical: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let inner = FlashblocksNodeExtensionsInner {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            fb_cell: Arc::new(OnceCell::new()),
            process_canonical,
        };
        Self { inner: Arc::new(inner) }
    }

    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let fb_cell = self.inner.fb_cell.clone();
        let receiver = self.inner.receiver.clone();
        let process_canonical = self.inner.process_canonical;

        let fb_cell_for_exex = fb_cell.clone();

        builder
            .install_exex("flashblocks-canon", move |mut ctx| {
                let fb_cell = fb_cell_for_exex.clone();
                let process_canonical = process_canonical;
                async move {
                    let provider = ctx.provider().clone();
                    let fb = init_flashblocks_state(&fb_cell, &provider);
                    Ok(async move {
                        while let Some(note) = ctx.notifications.try_next().await? {
                            if let Some(committed) = note.committed_chain() {
                                if process_canonical {
                                    // Many suites drive canonical updates manually to reproduce race conditions, so
                                    // allowing this to be disabled keeps canonical replay deterministic.
                                    for block in committed.blocks_iter() {
                                        fb.on_canonical_block_received(block);
                                    }
                                }
                                let _ = ctx
                                    .events
                                    .send(ExExEvent::FinishedHeight(committed.tip().num_hash()));
                            }
                        }
                        Ok(())
                    })
                }
            })
            .extend_rpc_modules(move |ctx| {
                let fb_cell = fb_cell.clone();
                let provider = ctx.provider().clone();
                let fb = init_flashblocks_state(&fb_cell, &provider);

                let mut canon_stream = tokio_stream::wrappers::BroadcastStream::new(
                    ctx.provider().subscribe_to_canonical_state(),
                );
                tokio::spawn(async move {
                    use tokio_stream::StreamExt;
                    while let Some(Ok(notification)) = canon_stream.next().await {
                        provider.canonical_in_memory_state().notify_canon_state(notification);
                    }
                });
                let api_ext = EthApiExt::new(
                    ctx.registry.eth_api().clone(),
                    ctx.registry.eth_handlers().filter.clone(),
                    fb.clone(),
                );
                ctx.modules.replace_configured(api_ext.into_rpc())?;

                // Register eth_subscribe subscription endpoint for flashblocks
                // Uses replace_configured since eth_subscribe already exists from reth's standard module
                let eth_pubsub = EthPubSub::new(fb.clone());
                ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

                let fb_for_task = fb.clone();
                let mut receiver = receiver
                    .lock()
                    .expect("flashblock receiver mutex poisoned")
                    .take()
                    .expect("flashblock receiver should only be initialized once");
                tokio::spawn(async move {
                    while let Some((payload, tx)) = receiver.recv().await {
                        fb_for_task.on_flashblock_received(payload);
                        let _ = tx.send(());
                    }
                });

                Ok(())
            })
    }

    fn wrap_launcher<L, LRet>(&self, launcher: L) -> impl FnOnce(OpBuilder) -> LRet
    where
        L: FnOnce(OpBuilder) -> LRet,
    {
        let extensions = self.clone();
        move |builder| {
            let builder = extensions.apply(builder);
            launcher(builder)
        }
    }

    fn parts(&self) -> Result<FlashblocksParts> {
        let state = self.inner.fb_cell.get().ok_or_else(|| {
            eyre::eyre!("FlashblocksState should be initialized during node launch")
        })?;
        Ok(FlashblocksParts { sender: self.inner.sender.clone(), state: state.clone() })
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

fn init_flashblocks_state(
    cell: &Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    provider: &LocalNodeProvider,
) -> Arc<LocalFlashblocksState> {
    cell.get_or_init(|| {
        let fb = Arc::new(FlashblocksState::new(provider.clone(), 5));
        fb.start();
        fb
    })
    .clone()
}

/// Local node wrapper that exposes helpers specific to Flashblocks tests.
pub struct FlashblocksLocalNode {
    node: LocalNode,
    parts: FlashblocksParts,
}

impl fmt::Debug for FlashblocksLocalNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksLocalNode")
            .field("node", &self.node)
            .field("parts", &self.parts)
            .finish()
    }
}

impl FlashblocksLocalNode {
    /// Launch a flashblocks-enabled node using the default launcher.
    pub async fn new() -> Result<Self> {
        Self::with_launcher(default_launcher).await
    }

    /// Builds a flashblocks-enabled node with canonical block streaming disabled so tests can call
    /// `FlashblocksState::on_canonical_block_received` at precise points.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_manual_canonical_launcher(default_launcher).await
    }

    /// Launch a flashblocks-enabled node with a custom launcher and canonical processing enabled.
    pub async fn with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        Self::with_launcher_inner(launcher, true).await
    }

    /// Same as [`Self::with_launcher`] but leaves canonical processing to the caller.
    pub async fn with_manual_canonical_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        Self::with_launcher_inner(launcher, false).await
    }

    async fn with_launcher_inner<L, LRet>(launcher: L, process_canonical: bool) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        let extensions = FlashblocksNodeExtensions::new(process_canonical);
        let wrapped_launcher = extensions.wrap_launcher(launcher);
        let node = LocalNode::new(wrapped_launcher).await?;

        let parts = extensions.parts()?;
        Ok(Self { node, parts })
    }

    /// Access the shared Flashblocks state for assertions or manual driving.
    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.parts.state()
    }

    /// Send a flashblock through the background processor and await completion.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    /// Split the wrapper into the underlying node plus flashblocks parts.
    pub fn into_parts(self) -> (LocalNode, FlashblocksParts) {
        (self.node, self.parts)
    }

    /// Borrow the underlying [`LocalNode`].
    pub fn as_node(&self) -> &LocalNode {
        &self.node
    }
}
