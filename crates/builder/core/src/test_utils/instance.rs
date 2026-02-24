use core::{
    any::Any,
    future::Future,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use std::sync::{Arc, LazyLock};

use alloy_primitives::B256;
use alloy_provider::{Identity, ProviderBuilder, RootProvider};
use base_alloy_network::Base;
use base_client_node::BaseNode;
use base_alloy_flashblocks::FlashblocksPayloadV1;
use futures::{FutureExt, StreamExt};
use nanoid::nanoid;
use parking_lot::Mutex;
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::{
    args::{DatadirArgs, NetworkArgs, RpcServerArgs},
    exit::NodeExitFuture,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{OpEngineValidatorBuilder, args::RollupArgs, node::OpPoolBuilder};
use reth_optimism_rpc::OpEthApiBuilder;
use reth_optimism_txpool::OpPooledTransaction;
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use reth_transaction_pool::{AllTransactionsEvents, TransactionPool};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::{
    BuilderConfig, SharedMeteringProvider,
    flashblocks::FlashblocksServiceBuilder,
    test_utils::{EngineApi, Ipc, TransactionPoolObserver, create_test_db, driver::ChainDriver},
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

/// Represents a type that emulates a local in-process instance of the OP builder node.
/// This node uses IPC as the communication channel for the RPC server Engine API.
#[derive(Debug)]
pub struct LocalInstance {
    node_config: NodeConfig<OpChainSpec>,
    builder_config: BuilderConfig,
    runtime: Option<Runtime>,
    exit_future: NodeExitFuture,
    _node_handle: Box<dyn Any + Send>,
    pool_observer: TransactionPoolObserver,
    metering_provider: SharedMeteringProvider,
}

impl LocalInstance {
    /// Creates a new local instance of the OP builder node with the given builder configuration,
    /// with the default Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new(builder_config: BuilderConfig) -> eyre::Result<Self> {
        Box::pin(Self::new_with_node_config(builder_config, default_node_config())).await
    }

    /// Creates a new local instance of the OP builder node with the given builder configuration,
    /// with a given Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new_with_node_config(
        builder_config: BuilderConfig,
        node_config: NodeConfig<OpChainSpec>,
    ) -> eyre::Result<Self> {
        clear_otel_env_vars();
        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;
        let base_node = BaseNode::new(RollupArgs::default());

        let (rpc_ready_tx, rpc_ready_rx) = oneshot::channel::<()>();
        let (txpool_ready_tx, txpool_ready_rx) =
            oneshot::channel::<AllTransactionsEvents<OpPooledTransaction>>();

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();
        let metering_provider = Arc::clone(&builder_config.metering_provider);

        let addons: base_client_node::BaseAddOns<_, OpEthApiBuilder, OpEngineValidatorBuilder> =
            base_node
                .add_ons_builder()
                .with_da_config(da_config.clone())
                .with_gas_limit_config(gas_limit_config.clone())
                .build();

        let node_builder = NodeBuilder::<_, OpChainSpec>::new(node_config.clone())
            .with_database(create_test_db(node_config.clone()))
            .with_launch_context(runtime.clone())
            .with_types::<BaseNode>()
            .with_components(
                base_node
                    .components()
                    .pool(pool_component())
                    .payload(FlashblocksServiceBuilder(builder_config.clone())),
            )
            .with_add_ons(addons)
            .on_rpc_started(move |_, _| {
                let _ = rpc_ready_tx.send(());
                Ok(())
            })
            .on_node_started(move |ctx| {
                txpool_ready_tx
                    .send(ctx.pool.all_transactions_event_listener())
                    .expect("Failed to send txpool ready signal");

                Ok(())
            });

        let node_handle = node_builder.launch().await?;
        let exit_future = node_handle.node_exit_future;
        let boxed_handle = Box::new(node_handle.node);
        let node_handle: Box<dyn Any + Send> = boxed_handle;

        // Wait for all required components to be ready
        rpc_ready_rx.await.expect("Failed to receive ready signal");
        let pool_monitor = txpool_ready_rx.await.expect("Failed to receive txpool ready signal");

        Ok(Self {
            builder_config,
            node_config,
            exit_future,
            _node_handle: node_handle,
            runtime: Some(runtime),
            pool_observer: TransactionPoolObserver::new(pool_monitor),
            metering_provider,
        })
    }

    /// Creates new local instance of the OP builder node with the flashblocks builder configuration.
    /// This method prefunds the default accounts with 1 ETH each.
    pub async fn flashblocks() -> eyre::Result<Self> {
        clear_otel_env_vars();
        Self::new(BuilderConfig::for_tests()).await
    }

    pub const fn node_config(&self) -> &NodeConfig<OpChainSpec> {
        &self.node_config
    }

    pub const fn builder_config(&self) -> &BuilderConfig {
        &self.builder_config
    }

    pub fn flashblocks_ws_url(&self) -> String {
        let ipaddr = self.builder_config.flashblocks.ws_addr.ip();
        let ipaddr = if ipaddr.is_unspecified() {
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            ipaddr
        };
        let port = self.builder_config.flashblocks.ws_addr.port();
        format!("ws://{ipaddr}:{port}/")
    }

    pub fn spawn_flashblocks_listener(&self) -> FlashblocksListener {
        FlashblocksListener::new(self.flashblocks_ws_url())
    }

    pub fn rpc_ipc(&self) -> &str {
        &self.node_config.rpc.ipcpath
    }

    pub fn auth_ipc(&self) -> &str {
        &self.node_config.rpc.auth_ipc_path
    }

    pub fn engine_api(&self) -> EngineApi<Ipc> {
        EngineApi::<Ipc>::with_ipc(self.auth_ipc())
    }

    pub const fn pool(&self) -> &TransactionPoolObserver {
        &self.pool_observer
    }

    pub fn metering_provider(&self) -> &SharedMeteringProvider {
        &self.metering_provider
    }

    pub async fn driver(&self) -> eyre::Result<ChainDriver<Ipc>> {
        ChainDriver::<Ipc>::local(self).await
    }

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
            runtime.graceful_shutdown_with_timeout(Duration::from_secs(3));
            std::fs::remove_dir_all(self.node_config().datadir().to_string()).unwrap_or_else(|e| {
                panic!(
                    "Failed to remove temporary data directory {}: {e}",
                    self.node_config().datadir()
                )
            });
        }
    }
}

impl Future for LocalInstance {
    type Output = eyre::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().exit_future.poll_unpin(cx)
    }
}

pub fn default_node_config() -> NodeConfig<OpChainSpec> {
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

    NodeConfig::<OpChainSpec>::new(chain_spec())
        .with_datadir_args(datadir)
        .with_rpc(rpc)
        .with_network(network)
}

fn chain_spec() -> Arc<OpChainSpec> {
    static CHAIN_SPEC: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
        let genesis = include_str!("./artifacts/genesis.json.tmpl");
        let genesis = serde_json::from_str(genesis).expect("invalid genesis JSON");
        let chain_spec = OpChainSpec::from_genesis(genesis);
        Arc::new(chain_spec)
    });

    CHAIN_SPEC.clone()
}

fn pool_component() -> OpPoolBuilder<OpPooledTransaction> {
    OpPoolBuilder::<OpPooledTransaction>::default()
}

/// A utility for listening to flashblocks WebSocket messages during tests.
///
/// This provides a reusable way to capture and inspect flashblocks that are produced
/// during test execution, eliminating the need for duplicate WebSocket listening code.
#[derive(Debug)]
pub struct FlashblocksListener {
    pub flashblocks: Arc<Mutex<Vec<FlashblocksPayloadV1>>>,
    pub cancellation_token: CancellationToken,
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
