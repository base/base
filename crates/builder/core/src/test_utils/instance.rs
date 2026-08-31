use core::{
    any::Any,
    future::Future,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use alloy_primitives::B256;
use alloy_provider::{Identity, ProviderBuilder, RootProvider};
use async_trait::async_trait;
use base_common_flashblocks::FlashblocksPayloadV1;
use base_common_network::Base;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::BasePooledTransaction;
use base_node_core::args::RollupArgs;
use base_node_runner::{
    BaseNode, BaseNodeExtension, FromExtensionConfig, NodeHooks,
    PayloadServiceBuilder as BasePayloadServiceBuilder, test_utils::init_silenced_tracing,
};
use futures::{FutureExt, StreamExt};
use nanoid::nanoid;
use parking_lot::Mutex;
use reth_node_builder::{Node, NodeBuilder, NodeConfig};
use reth_node_core::{
    args::{DatadirArgs, NetworkArgs, RpcServerArgs},
    exit::NodeExitFuture,
};
use reth_provider::providers::BlockchainProvider;
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use reth_transaction_pool::{AllTransactionsEvents, TransactionPool};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::{
    BuilderConfig, SharedMeteringProvider,
    flashblocks::FlashblocksServiceBuilder,
    test_utils::{
        EngineApi, Ipc, TransactionPoolObserver, create_test_db_env, driver::ChainDriver,
    },
};

/// Clears OTEL-related environment variables that can interfere with CLI argument parsing.
/// This is necessary because clap reads env vars for args with `env = "..."` attributes,
/// and external OTEL env vars (e.g., `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`) may contain
/// values that are incompatible with the CLI's expected values.
pub fn clear_otel_env_vars() {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_LOGS_EXPORTER",
        "OTEL_METRICS_EXPORTER",
        "OTEL_TRACES_EXPORTER",
        "OTEL_SDK_DISABLED",
    ] {
        // SAFETY: We're in a test environment where env var mutation is acceptable
        unsafe { std::env::remove_var(key) };
    }
}

/// Represents a type that emulates a local in-process instance of the builder node.
/// This node uses IPC as the communication channel for the RPC server Engine API.
#[derive(Debug)]
pub struct LocalInstance {
    node_config: NodeConfig<BaseChainSpec>,
    builder_config: BuilderConfig,
    runtime: Option<Runtime>,
    exit_future: NodeExitFuture,
    node_handle: Option<Box<dyn Any + Send>>,
    pool_handle: Option<Arc<dyn ExternalTransactionPool>>,
    pool_observer: TransactionPoolObserver,
    metering_provider: SharedMeteringProvider,
    /// Temporary directory backing the node's database, removed on drop.
    db_dir: PathBuf,
}

struct PoolHandle<P> {
    pool: P,
}

impl<P: core::fmt::Debug> core::fmt::Debug for PoolHandle<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PoolHandle").field("pool", &self.pool).finish()
    }
}

/// Trait for submitting transactions to the pool from outside the node.
#[async_trait]
pub trait ExternalTransactionPool: Send + Sync + core::fmt::Debug {
    /// Submits a pooled transaction as if it arrived from an external peer.
    async fn add_external_transaction(&self, tx: BasePooledTransaction) -> eyre::Result<()>;
}

#[async_trait]
impl<P> ExternalTransactionPool for PoolHandle<P>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + core::fmt::Debug,
{
    async fn add_external_transaction(&self, tx: BasePooledTransaction) -> eyre::Result<()> {
        TransactionPool::add_external_transaction(&self.pool, tx)
            .await
            .map(|_| ())
            .map_err(|err| eyre::eyre!("pool rejected transaction: {err}"))
    }
}

/// Builder for a [`LocalInstance`] that supports installing node extensions.
///
/// The resulting node is wired through the same payload-service and extension-hook pipeline used by
/// the production runner, so extensions installed here run exactly as they would in a real node.
///
/// ```ignore
/// let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
///     .install_ext::<MyExtension>(my_config)
///     .build()
///     .await?;
/// ```
#[derive(derive_more::Debug)]
pub struct LocalInstanceBuilder {
    builder_config: BuilderConfig,
    node_config: NodeConfig<BaseChainSpec>,
    #[debug("{}", extensions.len())]
    extensions: Vec<Box<dyn BaseNodeExtension>>,
}

impl LocalInstanceBuilder {
    /// Creates a new builder with the given builder configuration, the default node configuration,
    /// and no extensions.
    pub fn new(builder_config: BuilderConfig) -> Self {
        Self { builder_config, node_config: default_node_config(), extensions: Vec::new() }
    }
}

impl LocalInstanceBuilder {
    /// Overrides the Reth node configuration.
    #[must_use]
    pub fn with_node_config(mut self, node_config: NodeConfig<BaseChainSpec>) -> Self {
        self.node_config = node_config;
        self
    }

    /// Installs a node extension built from the given config, mirroring
    /// [`BaseNodeRunner::install_ext`](base_node_runner::BaseNodeRunner::install_ext).
    #[must_use]
    pub fn install_ext<T: FromExtensionConfig + 'static>(mut self, config: T::Config) -> Self {
        self.extensions.push(Box::new(T::from_config(config)));
        self
    }

    /// Installs an already-constructed node extension.
    #[must_use]
    pub fn with_extension(mut self, extension: Box<dyn BaseNodeExtension>) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Launches the node described by this builder and returns the running [`LocalInstance`].
    ///
    /// This method does not prefund any accounts, so before sending any transactions make sure that
    /// sender accounts are funded.
    pub async fn build(self) -> eyre::Result<LocalInstance> {
        Box::pin(LocalInstance::launch(self.builder_config, self.node_config, self.extensions))
            .await
    }
}

impl LocalInstance {
    /// Creates a new local instance of the builder node with the given builder configuration,
    /// with the default Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new(builder_config: BuilderConfig) -> eyre::Result<Self> {
        Box::pin(LocalInstanceBuilder::new(builder_config).build()).await
    }

    /// Creates a new local instance of the builder node with the given builder configuration,
    /// with a given Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new_with_node_config(
        builder_config: BuilderConfig,
        node_config: NodeConfig<BaseChainSpec>,
    ) -> eyre::Result<Self> {
        Box::pin(LocalInstanceBuilder::new(builder_config).with_node_config(node_config).build())
            .await
    }

    /// Core launch routine shared by all constructors.
    ///
    /// Builds the node through the runner's payload-service seam and applies caller-supplied
    /// [extensions](BaseNodeExtension) via the same [`NodeHooks`] pipeline used in production,
    /// plus an internal hook that captures the running node's transaction pool for tests.
    async fn launch(
        builder_config: BuilderConfig,
        node_config: NodeConfig<BaseChainSpec>,
        extensions: Vec<Box<dyn BaseNodeExtension>>,
    ) -> eyre::Result<Self> {
        clear_otel_env_vars();
        init_silenced_tracing();
        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();
        let metering_provider = Arc::clone(&builder_config.metering_provider);

        let base_node = BaseNode::new(RollupArgs::default())
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config);

        let service_builder = FlashblocksServiceBuilder::new(builder_config.clone());
        let components = service_builder.build_components(&base_node);

        let (txpool_ready_tx, txpool_ready_rx) =
            oneshot::channel::<AllTransactionsEvents<BasePooledTransaction>>();
        let (pool_handle_tx, pool_handle_rx) =
            oneshot::channel::<Arc<dyn ExternalTransactionPool>>();

        // The node types fix the database to the concrete `DatabaseEnv`, so the test database must
        // be a bare `DatabaseEnv` (not a `TempDatabase`) for the extension hook types to line up.
        let (db, db_dir) = create_test_db_env(node_config.clone())?;

        let builder = NodeBuilder::<_, BaseChainSpec>::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone())
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(components)
            .with_add_ons(base_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        // Apply caller-supplied extensions through the production hook pipeline, then append an
        // internal node-started hook that captures the running node's transaction pool.
        let hooks = extensions.into_iter().fold(NodeHooks::new(), |hooks, ext| ext.apply(hooks));
        let hooks = hooks.add_node_started_hook(move |full_node| {
            if txpool_ready_tx.send(full_node.pool.all_transactions_event_listener()).is_err() {
                tracing::warn!("txpool ready receiver dropped before node-started hook fired");
            }
            let pool_handle: Arc<dyn ExternalTransactionPool> =
                Arc::new(PoolHandle { pool: full_node.pool });
            if pool_handle_tx.send(pool_handle).is_err() {
                tracing::warn!("pool handle receiver dropped before node-started hook fired");
            }
            Ok(())
        });

        let node_handle = hooks.apply_to(builder).launch().await?;
        let exit_future = node_handle.node_exit_future;
        let node_handle: Box<dyn Any + Send> = Box::new(node_handle.node);

        // Wait for the node-started hook to publish the pool handles.
        let pool_monitor = txpool_ready_rx.await.expect("Failed to receive txpool ready signal");
        let pool_handle = pool_handle_rx.await.expect("Failed to receive pool handle");

        Ok(Self {
            builder_config,
            node_config,
            exit_future,
            node_handle: Some(node_handle),
            pool_handle: Some(pool_handle),
            runtime: Some(runtime),
            pool_observer: TransactionPoolObserver::new(pool_monitor),
            metering_provider,
            db_dir,
        })
    }

    /// Creates a new local instance of the builder node with the flashblocks builder configuration.
    /// This method prefunds the default accounts with 1 ETH each.
    pub async fn flashblocks() -> eyre::Result<Self> {
        clear_otel_env_vars();
        Self::new(BuilderConfig::for_tests()).await
    }

    /// Returns the Reth node configuration.
    pub const fn node_config(&self) -> &NodeConfig<BaseChainSpec> {
        &self.node_config
    }

    /// Returns the builder configuration.
    pub const fn builder_config(&self) -> &BuilderConfig {
        &self.builder_config
    }

    /// Returns the WebSocket URL for the flashblocks publisher.
    pub fn flashblocks_ws_url(&self) -> String {
        let ipaddr = self.builder_config.flashblocks_ws_addr.ip();
        let ipaddr = if ipaddr.is_unspecified() {
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            ipaddr
        };
        let port = self.builder_config.flashblocks_ws_addr.port();
        format!("ws://{ipaddr}:{port}/")
    }

    /// Spawns a background task that listens for flashblock payloads over WebSocket.
    pub fn spawn_flashblocks_listener(&self) -> FlashblocksListener {
        FlashblocksListener::new(self.flashblocks_ws_url())
    }

    /// Returns the IPC socket path for the regular JSON-RPC server.
    pub fn rpc_ipc(&self) -> &str {
        &self.node_config.rpc.ipcpath
    }

    /// Returns the IPC socket path for the authenticated Engine API server.
    pub fn auth_ipc(&self) -> &str {
        &self.node_config.rpc.auth_ipc_path
    }

    /// Creates an IPC-based [`EngineApi`] client for this instance.
    pub fn engine_api(&self) -> EngineApi<Ipc> {
        EngineApi::<Ipc>::with_ipc(self.auth_ipc())
    }

    /// Returns a reference to the transaction pool observer.
    pub const fn pool(&self) -> &TransactionPoolObserver {
        &self.pool_observer
    }

    /// Returns a cloned handle for submitting external transactions to the pool.
    pub fn pool_handle(&self) -> Arc<dyn ExternalTransactionPool> {
        Arc::clone(self.pool_handle.as_ref().expect("pool handle present"))
    }

    /// Returns a reference to the shared metering provider.
    pub fn metering_provider(&self) -> &SharedMeteringProvider {
        &self.metering_provider
    }

    /// Creates a [`ChainDriver`] connected to this local instance.
    pub async fn driver(&self) -> eyre::Result<ChainDriver<Ipc>> {
        ChainDriver::<Ipc>::local(self).await
    }

    /// Creates an alloy provider connected to this instance over IPC.
    pub async fn provider(&self) -> eyre::Result<RootProvider<Base>> {
        ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_ipc(self.rpc_ipc().to_string().into())
            .await
            .map_err(|e| eyre::eyre!("Failed to connect to provider: {e}"))
    }
}

impl Drop for LocalInstance {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Tokio runtimes cannot perform their blocking shutdown while they are being dropped
            // from another runtime's async context. `LocalInstance` is commonly owned directly by
            // async tests, so shut down and drop its runtime on a plain thread before cleaning up
            // the resources it owns.
            let shutdown = std::thread::spawn(move || {
                runtime.graceful_shutdown_with_timeout(Duration::from_secs(10));
                drop(runtime);
            });
            if let Err(panic) = shutdown.join() {
                std::panic::resume_unwind(panic);
            }
            // Drop the node and the pool handle (both hold open database handles via the node's
            // provider / the pool's transaction validator) before removing the backing files.
            drop(self.node_handle.take());
            drop(self.pool_handle.take());
            if let Err(e) = std::fs::remove_dir_all(self.node_config().datadir().to_string()) {
                eprintln!(
                    "Warning: failed to remove temporary data directory {}: {e}",
                    self.node_config().datadir()
                );
            }
            if let Err(e) = std::fs::remove_dir_all(&self.db_dir) {
                eprintln!(
                    "Warning: failed to remove temporary database directory {}: {e}",
                    self.db_dir.display()
                );
            }
        }
    }
}

impl Future for LocalInstance {
    type Output = eyre::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().exit_future.poll_unpin(cx)
    }
}

/// Returns the default Reth node configuration used in tests.
pub fn default_node_config() -> NodeConfig<BaseChainSpec> {
    node_config_with_chain_spec(chain_spec())
}

/// Returns the default test chain spec, lazily initialized from the embedded
/// genesis template.
pub fn chain_spec() -> Arc<BaseChainSpec> {
    static CHAIN_SPEC: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
        let genesis = include_str!("./artifacts/genesis.json.tmpl");
        let genesis = serde_json::from_str(genesis).expect("invalid genesis JSON");
        let chain_spec = BaseChainSpec::from_genesis(genesis);
        Arc::new(chain_spec)
    });

    CHAIN_SPEC.clone()
}

/// Returns a chain spec identical to the default test chain spec but with
/// `BaseUpgrade::Azul` activated at genesis (timestamp 0).
pub fn chain_spec_with_azul() -> Arc<BaseChainSpec> {
    use base_common_evm::BaseUpgrade;
    use reth_chainspec::ForkCondition;

    let genesis = include_str!("./artifacts/genesis.json.tmpl");
    let genesis = serde_json::from_str(genesis).expect("invalid genesis JSON");
    let mut spec = BaseChainSpec::from_genesis(genesis);
    spec.inner.hardforks.insert(BaseUpgrade::Azul, ForkCondition::Timestamp(0));
    Arc::new(spec)
}

/// Returns a node config using a chain spec with `BaseUpgrade::Azul` activated
/// at genesis.
pub fn default_node_config_with_azul() -> NodeConfig<BaseChainSpec> {
    node_config_with_chain_spec(chain_spec_with_azul())
}

/// Builds a [`LocalInstance`]-style Reth node configuration for the given chain spec.
///
/// Uses the same IPC-only RPC setup, disabled discovery, unused ports, and temporary data
/// directories as [`default_node_config`], but with a caller-supplied chain spec — so an in-process
/// builder node can be launched against a custom genesis (e.g. one derived from a rollup config).
pub fn node_config_with_chain_spec(spec: Arc<BaseChainSpec>) -> NodeConfig<BaseChainSpec> {
    let tempdir = std::env::temp_dir();
    let random_id = nanoid!();

    let data_path = tempdir.join(format!("rbuilder.{random_id}.datadir"));
    let rocksdb_path = tempdir.join(format!("rbuilder.{random_id}.rocksdb"));
    let pprof_dumps_path = tempdir.join(format!("rbuilder.{random_id}.pprof-dumps"));

    std::fs::create_dir_all(&data_path).expect("Failed to create temporary data directory");
    std::fs::create_dir_all(&rocksdb_path).expect("Failed to create temporary rocksdb directory");
    std::fs::create_dir_all(&pprof_dumps_path)
        .expect("Failed to create temporary pprof dumps directory");

    let rpc_ipc_path = tempdir.join(format!("rbuilder.{random_id}.rpc-ipc"));
    let auth_ipc_path = tempdir.join(format!("rbuilder.{random_id}.auth-ipc"));

    let mut rpc = RpcServerArgs::default().with_auth_ipc();
    rpc.ws = false;
    rpc.http = false;
    rpc.auth_port = 0;
    rpc.ipcpath = rpc_ipc_path.to_string_lossy().into();
    rpc.auth_ipc_path = auth_ipc_path.to_string_lossy().into();

    let mut network = NetworkArgs::default().with_unused_ports();
    network.discovery.disable_discovery = true;

    let datadir = DatadirArgs {
        datadir: data_path.to_string_lossy().parse().expect("Failed to parse data dir path"),
        static_files_path: None,
        rocksdb_path: Some(rocksdb_path),
        pprof_dumps_path: Some(pprof_dumps_path),
    };

    NodeConfig::<BaseChainSpec>::new(spec)
        .with_datadir_args(datadir)
        .with_rpc(rpc)
        .with_network(network)
}

/// A utility for listening to flashblocks WebSocket messages during tests.
///
/// This provides a reusable way to capture and inspect flashblocks that are produced
/// during test execution, eliminating the need for duplicate WebSocket listening code.
#[derive(Debug)]
pub struct FlashblocksListener {
    /// All flashblock payloads received so far.
    pub flashblocks: Arc<Mutex<Vec<FlashblocksPayloadV1>>>,
    /// Token used to signal the listener task to stop.
    pub cancellation_token: CancellationToken,
    /// Handle to the spawned listener task.
    pub handle: JoinHandle<eyre::Result<()>>,
}

impl FlashblocksListener {
    /// Create a new flashblocks listener that connects to the given WebSocket URL.
    ///
    /// The listener will automatically parse incoming messages as `FlashblocksPayloadV1`.
    fn new(flashblocks_ws_url: String) -> Self {
        let flashblocks = Arc::new(Mutex::new(Vec::new()));
        let cancellation_token = CancellationToken::new();

        let flashblocks_clone = Arc::clone(&flashblocks);
        let cancellation_token_clone = cancellation_token.clone();

        let handle = tokio::spawn(async move {
            let (ws_stream, _) = connect_async(flashblocks_ws_url).await?;
            let (_, mut read) = ws_stream.split();

            loop {
                tokio::select! {
                    _ = cancellation_token_clone.cancelled() => {
                        break Ok(());
                    }
                    Some(Ok(Message::Text(text))) = read.next() => {
                        let fb = serde_json::from_str(&text).unwrap();
                        flashblocks_clone.lock().push(fb);
                    }
                }
            }
        });

        Self { flashblocks, cancellation_token, handle }
    }

    /// Get a snapshot of all received flashblocks
    pub fn get_flashblocks(&self) -> Vec<FlashblocksPayloadV1> {
        self.flashblocks.lock().clone()
    }

    /// Find a flashblock by index
    pub fn find_flashblock(&self, index: u64) -> Option<FlashblocksPayloadV1> {
        self.flashblocks.lock().iter().find(|fb| fb.index == index).cloned()
    }

    /// Check if any flashblock contains the given transaction hash
    pub fn contains_transaction(&self, tx_hash: &B256) -> bool {
        let tx_hash_str = format!("{tx_hash:#x}");
        self.flashblocks.lock().iter().any(|fb| {
            if let Some(receipts) = fb.metadata.get("receipts")
                && let Some(receipts_obj) = receipts.as_object()
            {
                return receipts_obj.contains_key(&tx_hash_str);
            }
            false
        })
    }

    /// Find which flashblock index contains the given transaction hash
    pub fn find_transaction_flashblock(&self, tx_hash: &B256) -> Option<u64> {
        let tx_hash_str = format!("{tx_hash:#x}");
        self.flashblocks.lock().iter().find_map(|fb| {
            if let Some(receipts) = fb.metadata.get("receipts")
                && let Some(receipts_obj) = receipts.as_object()
                && receipts_obj.contains_key(&tx_hash_str)
            {
                return Some(fb.index);
            }
            None
        })
    }

    /// Stop the listener and wait for it to complete
    pub async fn stop(self) -> eyre::Result<()> {
        self.cancellation_token.cancel();
        self.handle.await?
    }
}
