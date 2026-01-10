//! Local node setup with Base Sepolia chainspec

use std::{
    any::Any,
    fmt,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use alloy_genesis::Genesis;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use base_client_primitives::{BaseNodeExtension, OpBuilder, OpProvider};
use base_flashblocks::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksReceiver,
    FlashblocksState,
};
use base_flashtypes::Flashblock;
use eyre::Result;
use once_cell::sync::OnceCell;
use op_alloy_network::Optimism;
use reth::{
    args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
    builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle, TreeConfig},
    core::exit::NodeExitFuture,
    providers::providers::BlockchainProvider,
    tasks::TaskManager,
};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db, mdbx::DatabaseArguments, test_utils::tempdir_path,
};
use reth_exex::ExExEvent;
use reth_node_core::{
    args::DatadirArgs,
    dirs::{DataDirPath, MaybePlatformPath},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{OpNode, args::RollupArgs};
use reth_provider::CanonStateSubscriptions;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;

use crate::engine::EngineApi;

/// Convenience alias for the local blockchain provider type.
pub type LocalNodeProvider = OpProvider;
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
    _db_path: PathBuf,
}

impl Drop for LocalNode {
    fn drop(&mut self) {
        // Clean up the temporary database directory
        let _ = std::fs::remove_dir_all(&self._db_path);
    }
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

/// Test extension for flashblocks functionality.
///
/// This extension wires up the flashblocks ExEx and RPC modules for testing,
/// with optional control over canonical block processing.
#[derive(Clone, Debug)]
pub struct FlashblocksTestExtension {
    inner: Arc<FlashblocksTestExtensionInner>,
}

struct FlashblocksTestExtensionInner {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    #[allow(clippy::type_complexity)]
    receiver: Arc<Mutex<Option<mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>>>>,
    fb_cell: Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    process_canonical: bool,
}

impl fmt::Debug for FlashblocksTestExtensionInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksTestExtensionInner")
            .field("process_canonical", &self.process_canonical)
            .finish_non_exhaustive()
    }
}

impl FlashblocksTestExtension {
    /// Create a new flashblocks test extension.
    ///
    /// If `process_canonical` is true, canonical blocks are automatically processed.
    /// Set to false for tests that need manual control over canonical block timing.
    pub fn new(process_canonical: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let inner = FlashblocksTestExtensionInner {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            fb_cell: Arc::new(OnceCell::new()),
            process_canonical,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Get the flashblocks parts after the node has been launched.
    pub fn parts(&self) -> Result<FlashblocksParts> {
        let state = self.inner.fb_cell.get().ok_or_else(|| {
            eyre::eyre!("FlashblocksState should be initialized during node launch")
        })?;
        Ok(FlashblocksParts { sender: self.inner.sender.clone(), state: state.clone() })
    }
}

impl BaseNodeExtension for FlashblocksTestExtension {
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let fb_cell = self.inner.fb_cell.clone();
        let receiver = self.inner.receiver.clone();
        let process_canonical = self.inner.process_canonical;

        let fb_cell_for_exex = fb_cell.clone();

        builder
            .install_exex("flashblocks-canon", move |mut ctx| {
                let fb_cell = fb_cell_for_exex.clone();
                async move {
                    let provider = ctx.provider().clone();
                    let fb = init_flashblocks_state(&fb_cell, &provider);
                    Ok(async move {
                        while let Some(note) = ctx.notifications.try_next().await? {
                            if let Some(committed) = note.committed_chain() {
                                let hash = committed.tip().num_hash();
                                if process_canonical {
                                    // Many suites drive canonical updates manually to reproduce race conditions, so
                                    // allowing this to be disabled keeps canonical replay deterministic.
                                    let chain = Arc::unwrap_or_clone(committed);
                                    for (_, block) in chain.into_blocks() {
                                        fb.on_canonical_block_received(block);
                                    }
                                }
                                let _ = ctx.events.send(ExExEvent::FinishedHeight(hash));
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
                // Pass eth_api to enable proxying standard subscription types to reth's implementation
                let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), fb.clone());
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
}

impl LocalNode {
    /// Launch a new local node with the provided extensions.
    pub async fn new(extensions: Vec<Box<dyn BaseNodeExtension>>) -> Result<Self> {
        build_node(extensions).await
    }

    /// Creates a test database with a smaller map size to reduce memory usage.
    ///
    /// Unlike `NodeBuilder::testing_node()` which hardcodes an 8 TB map size,
    /// this method configures the database with a 100 MB map size. This prevents
    /// `ENOMEM` errors when running parallel tests with `cargo test`, as the
    /// default 8 TB size can cause memory exhaustion when multiple test processes
    /// run concurrently.
    ///
    /// Returns the database wrapped in Arc and the path for cleanup.
    fn create_test_database() -> Result<(Arc<DatabaseEnv>, PathBuf)> {
        let default_size = 100 * 1024 * 1024; // 100 MB
        Self::create_test_database_with_size(default_size)
    }

    /// Creates a test database with a configurable map size to reduce memory usage.
    ///
    /// # Arguments
    ///
    /// * `max_size` - Maximum map size in bytes.
    fn create_test_database_with_size(max_size: usize) -> Result<(Arc<DatabaseEnv>, PathBuf)> {
        let path = tempdir_path();
        let emsg = format!("Failed to create test database at {path:?}");
        let args =
            DatabaseArguments::new(ClientVersion::default()).with_geometry_max_size(Some(max_size));
        let db = init_db(&path, args).expect(&emsg);
        Ok((Arc::new(db), path))
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

async fn build_node(extensions: Vec<Box<dyn BaseNodeExtension>>) -> Result<LocalNode> {
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

    let op_node = OpNode::new(RollupArgs::default());

    let (db, db_path) = LocalNode::create_test_database()?;

    let mut node_config = NodeConfig::new(chain_spec.clone())
        .with_network(network_config)
        .with_rpc(rpc_args)
        .with_unused_ports();

    let datadir_path = MaybePlatformPath::<DataDirPath>::from(db_path.clone());
    node_config =
        node_config.with_datadir_args(DatadirArgs { datadir: datadir_path, ..Default::default() });

    let builder = NodeBuilder::new(node_config.clone())
        .with_database(db)
        .with_launch_context(exec.clone())
        .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
        .with_components(op_node.components())
        .with_add_ons(op_node.add_ons())
        .on_component_initialized(move |_ctx| Ok(()));

    // Apply all extensions
    let builder =
        extensions.into_iter().fold(builder, |builder, extension| extension.apply(builder));

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
        _db_path: db_path,
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
    /// Launch a flashblocks-enabled node using the default configuration.
    pub async fn new() -> Result<Self> {
        Self::with_options(true).await
    }

    /// Builds a flashblocks-enabled node with canonical block streaming disabled so tests can call
    /// `FlashblocksState::on_canonical_block_received` at precise points.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_options(false).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();
        let node = LocalNode::new(vec![Box::new(extension)]).await?;
        let parts = parts_source.parts()?;
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
}
