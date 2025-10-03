use crate::{
    args::OpRbuilderArgs,
    builders::{BuilderConfig, FlashblocksBuilder, PayloadBuilder, StandardBuilder},
    primitives::reth::engine_api_builder::OpEngineApiBuilder,
    revert_protection::{EthApiExtServer, RevertProtectionExt},
    tests::{
        EngineApi, Ipc, TransactionPoolObserver, builder_signer, create_test_db,
        framework::driver::ChainDriver,
    },
    tx::FBPooledTransaction,
    tx_signer::Signer,
};
use alloy_primitives::B256;
use alloy_provider::{Identity, ProviderBuilder, RootProvider};
use clap::Parser;
use core::{
    any::Any,
    future::Future,
    net::Ipv4Addr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use futures::{FutureExt, StreamExt};
use moka::future::Cache;
use nanoid::nanoid;
use op_alloy_network::Optimism;
use parking_lot::Mutex;
use reth::{
    args::{DatadirArgs, NetworkArgs, RpcServerArgs},
    core::exit::NodeExitFuture,
    tasks::TaskManager,
};
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::commands::Commands;
use reth_optimism_node::{
    OpNode,
    node::{OpAddOns, OpAddOnsBuilder, OpEngineValidatorBuilder, OpPoolBuilder},
};
use reth_optimism_rpc::OpEthApiBuilder;
use reth_transaction_pool::{AllTransactionsEvents, TransactionPool};
use rollup_boost::FlashblocksPayloadV1;
use std::sync::{Arc, LazyLock};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Represents a type that emulates a local in-process instance of the OP builder node.
/// This node uses IPC as the communication channel for the RPC server Engine API.
pub struct LocalInstance {
    signer: Signer,
    config: NodeConfig<OpChainSpec>,
    args: OpRbuilderArgs,
    task_manager: Option<TaskManager>,
    exit_future: NodeExitFuture,
    _node_handle: Box<dyn Any + Send>,
    pool_observer: TransactionPoolObserver,
}

impl LocalInstance {
    /// Creates a new local instance of the OP builder node with the given arguments,
    /// with the default Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new<P: PayloadBuilder>(args: OpRbuilderArgs) -> eyre::Result<Self> {
        Box::pin(Self::new_with_config::<P>(args, default_node_config())).await
    }

    /// Creates a new local instance of the OP builder node with the given arguments,
    /// with a given Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new_with_config<P: PayloadBuilder>(
        args: OpRbuilderArgs,
        config: NodeConfig<OpChainSpec>,
    ) -> eyre::Result<Self> {
        let mut args = args;
        let task_manager = task_manager();
        let op_node = OpNode::new(args.rollup_args.clone());
        let reverted_cache = Cache::builder().max_capacity(100).build();
        let reverted_cache_clone = reverted_cache.clone();

        let (rpc_ready_tx, rpc_ready_rx) = oneshot::channel::<()>();
        let (txpool_ready_tx, txpool_ready_rx) =
            oneshot::channel::<AllTransactionsEvents<FBPooledTransaction>>();

        let signer = args.builder_signer.unwrap_or(builder_signer());
        args.builder_signer = Some(signer);
        args.rollup_args.enable_tx_conditional = true;

        let builder_config = BuilderConfig::<P::Config>::try_from(args.clone())
            .expect("Failed to convert rollup args to builder config");
        let da_config = builder_config.da_config.clone();

        let addons: OpAddOns<
            _,
            OpEthApiBuilder,
            OpEngineValidatorBuilder,
            OpEngineApiBuilder<OpEngineValidatorBuilder>,
        > = OpAddOnsBuilder::default()
            .with_sequencer(args.rollup_args.sequencer.clone())
            .with_enable_tx_conditional(args.rollup_args.enable_tx_conditional)
            .with_da_config(da_config)
            .build();

        let node_builder = NodeBuilder::<_, OpChainSpec>::new(config.clone())
            .with_database(create_test_db(config.clone()))
            .with_launch_context(task_manager.executor())
            .with_types::<OpNode>()
            .with_components(
                op_node
                    .components()
                    .pool(pool_component(&args))
                    .payload(P::new_service(builder_config)?),
            )
            .with_add_ons(addons)
            .extend_rpc_modules(move |ctx| {
                if args.enable_revert_protection {
                    tracing::info!("Revert protection enabled");

                    let pool = ctx.pool().clone();
                    let provider = ctx.provider().clone();
                    let revert_protection_ext = RevertProtectionExt::new(
                        pool,
                        provider,
                        ctx.registry.eth_api().clone(),
                        reverted_cache,
                    );

                    ctx.modules
                        .add_or_replace_configured(revert_protection_ext.into_rpc())?;
                }

                Ok(())
            })
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
        let pool_monitor = txpool_ready_rx
            .await
            .expect("Failed to receive txpool ready signal");

        Ok(Self {
            args,
            signer,
            config,
            exit_future,
            _node_handle: node_handle,
            task_manager: Some(task_manager),
            pool_observer: TransactionPoolObserver::new(pool_monitor, reverted_cache_clone),
        })
    }

    /// Creates new local instance of the OP builder node with the standard builder configuration.
    /// This method prefunds the default accounts with 1 ETH each.
    pub async fn standard() -> eyre::Result<Self> {
        let args = crate::args::Cli::parse_from(["dummy", "node"]);
        let Commands::Node(ref node_command) = args.command else {
            unreachable!()
        };
        Self::new::<StandardBuilder>(node_command.ext.clone()).await
    }

    /// Creates new local instance of the OP builder node with the flashblocks builder configuration.
    /// This method prefunds the default accounts with 1 ETH each.
    pub async fn flashblocks() -> eyre::Result<Self> {
        let mut args = crate::args::Cli::parse_from(["dummy", "node"]);
        let Commands::Node(ref mut node_command) = args.command else {
            unreachable!()
        };
        node_command.ext.flashblocks.enabled = true;
        node_command.ext.flashblocks.flashblocks_port = 0; // use random os assigned port
        Self::new::<FlashblocksBuilder>(node_command.ext.clone()).await
    }

    pub const fn config(&self) -> &NodeConfig<OpChainSpec> {
        &self.config
    }

    pub const fn args(&self) -> &OpRbuilderArgs {
        &self.args
    }

    pub const fn signer(&self) -> &Signer {
        &self.signer
    }

    pub fn flashblocks_ws_url(&self) -> String {
        let ipaddr: Ipv4Addr = self
            .args
            .flashblocks
            .flashblocks_addr
            .parse()
            .expect("Failed to parse flashblocks IP address");

        let ipaddr = if ipaddr.is_unspecified() {
            Ipv4Addr::LOCALHOST
        } else {
            ipaddr
        };

        let port = self.args.flashblocks.flashblocks_port;

        format!("ws://{ipaddr}:{port}/")
    }

    pub fn spawn_flashblocks_listener(&self) -> FlashblocksListener {
        FlashblocksListener::new(self.flashblocks_ws_url())
    }

    pub fn rpc_ipc(&self) -> &str {
        &self.config.rpc.ipcpath
    }

    pub fn auth_ipc(&self) -> &str {
        &self.config.rpc.auth_ipc_path
    }

    pub fn engine_api(&self) -> EngineApi<Ipc> {
        EngineApi::<Ipc>::with_ipc(self.auth_ipc())
    }

    pub const fn pool(&self) -> &TransactionPoolObserver {
        &self.pool_observer
    }

    pub async fn driver(&self) -> eyre::Result<ChainDriver<Ipc>> {
        ChainDriver::<Ipc>::local(self).await
    }

    pub async fn provider(&self) -> eyre::Result<RootProvider<Optimism>> {
        ProviderBuilder::<Identity, Identity, Optimism>::default()
            .connect_ipc(self.rpc_ipc().to_string().into())
            .await
            .map_err(|e| eyre::eyre!("Failed to connect to provider: {e}"))
    }
}

impl Drop for LocalInstance {
    fn drop(&mut self) {
        if let Some(task_manager) = self.task_manager.take() {
            task_manager.graceful_shutdown_with_timeout(Duration::from_secs(3));
            std::fs::remove_dir_all(self.config().datadir().to_string()).unwrap_or_else(|e| {
                panic!(
                    "Failed to remove temporary data directory {}: {e}",
                    self.config().datadir()
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

    let data_path = tempdir
        .join(format!("rbuilder.{random_id}.datadir"))
        .to_path_buf();

    std::fs::create_dir_all(&data_path).expect("Failed to create temporary data directory");

    let rpc_ipc_path = tempdir
        .join(format!("rbuilder.{random_id}.rpc-ipc"))
        .to_path_buf();

    let auth_ipc_path = tempdir
        .join(format!("rbuilder.{random_id}.auth-ipc"))
        .to_path_buf();

    let mut rpc = RpcServerArgs::default().with_auth_ipc();
    rpc.ws = false;
    rpc.http = false;
    rpc.auth_port = 0;
    rpc.ipcpath = rpc_ipc_path.to_string_lossy().into();
    rpc.auth_ipc_path = auth_ipc_path.to_string_lossy().into();

    let mut network = NetworkArgs::default().with_unused_ports();
    network.discovery.disable_discovery = true;

    let datadir = DatadirArgs {
        datadir: data_path
            .to_string_lossy()
            .parse()
            .expect("Failed to parse data dir path"),
        static_files_path: None,
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

fn task_manager() -> TaskManager {
    TaskManager::new(tokio::runtime::Handle::current())
}

fn pool_component(args: &OpRbuilderArgs) -> OpPoolBuilder<FBPooledTransaction> {
    let rollup_args = &args.rollup_args;
    OpPoolBuilder::<FBPooledTransaction>::default()
        .with_enable_tx_conditional(
            // Revert protection uses the same internal pool logic as conditional transactions
            // to garbage collect transactions out of the bundle range.
            rollup_args.enable_tx_conditional || args.enable_revert_protection,
        )
        .with_supervisor(
            rollup_args.supervisor_http.clone(),
            rollup_args.supervisor_safety_level,
        )
}

/// A utility for listening to flashblocks WebSocket messages during tests.
///
/// This provides a reusable way to capture and inspect flashblocks that are produced
/// during test execution, eliminating the need for duplicate WebSocket listening code.
pub struct FlashblocksListener {
    pub flashblocks: Arc<Mutex<Vec<FlashblocksPayloadV1>>>,
    pub cancellation_token: CancellationToken,
    pub handle: JoinHandle<eyre::Result<()>>,
}

impl FlashblocksListener {
    /// Create a new flashblocks listener that connects to the given WebSocket URL.
    ///
    /// The listener will automatically parse incoming messages as FlashblocksPayloadV1.
    fn new(flashblocks_ws_url: String) -> Self {
        let flashblocks = Arc::new(Mutex::new(Vec::new()));
        let cancellation_token = CancellationToken::new();

        let flashblocks_clone = flashblocks.clone();
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
                        warn!("GOT FB: {fb:#?}");
                        flashblocks_clone.lock().push(fb);
                    }
                }
            }
        });

        Self {
            flashblocks,
            cancellation_token,
            handle,
        }
    }

    /// Get a snapshot of all received flashblocks
    pub fn get_flashblocks(&self) -> Vec<FlashblocksPayloadV1> {
        self.flashblocks.lock().clone()
    }

    /// Find a flashblock by index
    pub fn find_flashblock(&self, index: u64) -> Option<FlashblocksPayloadV1> {
        self.flashblocks
            .lock()
            .iter()
            .find(|fb| fb.index == index)
            .cloned()
    }

    /// Check if any flashblock contains the given transaction hash
    pub fn contains_transaction(&self, tx_hash: &B256) -> bool {
        let tx_hash_str = format!("{tx_hash:#x}");
        self.flashblocks.lock().iter().any(|fb| {
            if let Some(receipts) = fb.metadata.get("receipts") {
                if let Some(receipts_obj) = receipts.as_object() {
                    return receipts_obj.contains_key(&tx_hash_str);
                }
            }
            false
        })
    }

    /// Find which flashblock index contains the given transaction hash
    pub fn find_transaction_flashblock(&self, tx_hash: &B256) -> Option<u64> {
        let tx_hash_str = format!("{tx_hash:#x}");
        self.flashblocks.lock().iter().find_map(|fb| {
            if let Some(receipts) = fb.metadata.get("receipts") {
                if let Some(receipts_obj) = receipts.as_object() {
                    if receipts_obj.contains_key(&tx_hash_str) {
                        return Some(fb.index);
                    }
                }
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
