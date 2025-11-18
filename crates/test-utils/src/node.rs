//! Local node setup with Base Sepolia chainspec

use crate::engine::EngineApi;
use alloy_genesis::Genesis;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use base_reth_flashblocks_rpc::rpc::{EthApiExt, EthApiOverrideServer};
use base_reth_flashblocks_rpc::state::FlashblocksState;
use base_reth_flashblocks_rpc::subscription::{Flashblock, FlashblocksReceiver};
use eyre::Result;
use futures_util::Future;
use once_cell::sync::OnceCell;
use op_alloy_network::Optimism;
use reth::api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter};
use reth::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use reth::builder::{
    Node, NodeBuilder, NodeBuilderWithComponents, NodeConfig, NodeHandle, WithLaunchContext,
};
use reth::core::exit::NodeExitFuture;
use reth::tasks::TaskManager;
use reth_e2e_test_utils::{Adapter, TmpDB};
use reth_exex::ExExEvent;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;
use reth_provider::providers::BlockchainProvider;
use reth_provider::CanonStateSubscriptions;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;

pub const BASE_CHAIN_ID: u64 = 84532;

pub type LocalNodeProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, TmpDB>>;
pub type LocalFlashblocksState = FlashblocksState<LocalNodeProvider>;

pub struct LocalNode {
    pub(crate) http_api_addr: SocketAddr,
    engine_ipc_path: String,
    flashblock_sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    flashblocks_state: Arc<LocalFlashblocksState>,
    provider: LocalNodeProvider,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _task_manager: TaskManager,
}

pub type OpTypes =
    FullNodeTypesAdapter<OpNode, TmpDB, BlockchainProvider<NodeTypesWithDBAdapter<OpNode, TmpDB>>>;
pub type OpComponentsBuilder = <OpNode as Node<OpTypes>>::ComponentsBuilder;
pub type OpAddOns = <OpNode as Node<OpTypes>>::AddOns;
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpTypes, OpComponentsBuilder, OpAddOns>>;

pub async fn default_launcher(
    builder: OpBuilder,
) -> eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>> {
    let launcher = builder.engine_api_launcher();
    builder.launch_with(launcher).await
}

impl LocalNode {
    pub async fn new<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        let tasks = TaskManager::current();
        let exec = tasks.executor();

        let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))?;
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs {
                disable_discovery: true,
                ..DiscoveryArgs::default()
            },
            ..NetworkArgs::default()
        };

        // Generate unique IPC path for this test instance to avoid conflicts
        // Use timestamp + thread ID + process ID for uniqueness
        let unique_ipc_path = format!(
            "/tmp/reth_engine_api_{}_{}_{:?}.ipc",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id(),
            std::thread::current().id()
        );

        let mut rpc_args = RpcServerArgs::default()
            .with_unused_ports()
            .with_http()
            .with_auth_ipc();
        rpc_args.auth_ipc_path = unique_ipc_path;

        let node_config = NodeConfig::new(chain_spec.clone())
            .with_network(network_config)
            .with_rpc(rpc_args)
            .with_unused_ports();

        let node = OpNode::new(RollupArgs::default());

        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());
        let provider_cell: Arc<OnceCell<LocalNodeProvider>> = Arc::new(OnceCell::new());

        let NodeHandle {
            node: node_handle,
            node_exit_future,
        } = NodeBuilder::new(node_config.clone())
            .testing_node(exec.clone())
            .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
            .with_components(node.components_builder())
            .with_add_ons(node.add_ons())
            .install_exex("flashblocks-canon", {
                let fb_cell = fb_cell.clone();
                let provider_cell = provider_cell.clone();
                move |mut ctx| async move {
                    let provider = provider_cell.get_or_init(|| ctx.provider().clone()).clone();
                    let fb = init_flashblocks_state(&fb_cell, &provider);
                    Ok(async move {
                        while let Some(note) = ctx.notifications.try_next().await? {
                            if let Some(committed) = note.committed_chain() {
                                for block in committed.blocks_iter() {
                                    fb.on_canonical_block_received(block);
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
            .extend_rpc_modules({
                let fb_cell = fb_cell.clone();
                let provider_cell = provider_cell.clone();
                let mut receiver = Some(receiver);
                move |ctx| {
                    let provider = provider_cell.get_or_init(|| ctx.provider().clone()).clone();
                    let fb = init_flashblocks_state(&fb_cell, &provider);

                    let provider_for_task = provider.clone();
                    let mut canon_stream = tokio_stream::wrappers::BroadcastStream::new(
                        ctx.provider().subscribe_to_canonical_state(),
                    );
                    tokio::spawn(async move {
                        use tokio_stream::StreamExt;
                        while let Some(Ok(notification)) = canon_stream.next().await {
                            provider_for_task
                                .canonical_in_memory_state()
                                .notify_canon_state(notification);
                        }
                    });
                    let api_ext = EthApiExt::new(
                        ctx.registry.eth_api().clone(),
                        ctx.registry.eth_handlers().filter.clone(),
                        fb.clone(),
                    );
                    ctx.modules.replace_configured(api_ext.into_rpc())?;
                    // Spawn task to receive flashblocks from the test context
                    let fb_for_task = fb.clone();
                    let mut receiver = receiver
                        .take()
                        .expect("flashblock receiver should only be initialized once");
                    tokio::spawn(async move {
                        while let Some((payload, tx)) = receiver.recv().await {
                            fb_for_task.on_flashblock_received(payload);
                            let _ = tx.send(());
                        }
                    });
                    Ok(())
                }
            })
            .launch_with_fn(launcher)
            .await?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre::eyre!("HTTP RPC server failed to bind to address"))?;

        let engine_ipc_path = node_config.rpc.auth_ipc_path;
        let flashblocks_state = fb_cell
            .get()
            .expect("FlashblocksState should be initialized during node launch")
            .clone();
        let provider = provider_cell
            .get()
            .expect("Provider should be initialized during node launch")
            .clone();

        Ok(Self {
            http_api_addr,
            engine_ipc_path,
            flashblock_sender: sender,
            flashblocks_state,
            provider,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _task_manager: tasks,
        })
    }

    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.flashblock_sender
            .send((flashblock, tx))
            .await
            .map_err(|err| eyre::eyre!(err))?;
        rx.await.map_err(|err| eyre::eyre!(err))?;
        Ok(())
    }

    pub fn provider(&self) -> Result<RootProvider<Optimism>> {
        let url = format!("http://{}", self.http_api_addr);
        let client = RpcClient::builder().http(url.parse()?);
        Ok(RootProvider::<Optimism>::new(client))
    }

    pub fn engine_api(&self) -> Result<EngineApi<crate::engine::IpcEngine>> {
        EngineApi::<crate::engine::IpcEngine>::new(self.engine_ipc_path.clone())
    }

    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.flashblocks_state.clone()
    }

    pub fn blockchain_provider(&self) -> LocalNodeProvider {
        self.provider.clone()
    }
}

fn init_flashblocks_state(
    cell: &Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    provider: &LocalNodeProvider,
) -> Arc<LocalFlashblocksState> {
    cell.get_or_init(|| {
        let fb = Arc::new(FlashblocksState::new(provider.clone()));
        fb.start();
        fb
    })
    .clone()
}
