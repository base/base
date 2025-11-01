use alloy_consensus::Receipt;
use alloy_genesis::Genesis;
use alloy_primitives::map::HashMap;
use alloy_primitives::{address, b256, bytes, Address, Bytes, B256};
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::PayloadId;
use base_reth_flashblocks_rpc::rpc::{EthApiExt, EthApiOverrideServer};
use base_reth_flashblocks_rpc::state::FlashblocksState;
use base_reth_flashblocks_rpc::subscription::{Flashblock, FlashblocksReceiver, Metadata};
use op_alloy_consensus::OpDepositReceipt;
use op_alloy_network::Optimism;
use reth::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use reth::builder::{Node, NodeBuilder, NodeConfig, NodeHandle};
use reth::chainspec::Chain;
use reth::core::exit::NodeExitFuture;
use reth::tasks::TaskManager;
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpReceipt;
use reth_provider::providers::BlockchainProvider;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use std::any::Any;
use std::net::SocketAddr;
use std::sync::{Arc, Once};
use tokio::sync::{mpsc, oneshot};

pub const ALICE: Address = address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
pub const BOB: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");
pub const CHARLIE: Address = address!("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

pub const BLOCK_INFO_TXN: Bytes = bytes!("0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000");
pub const BLOCK_INFO_TXN_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");

pub struct NodeContext {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    http_api_addr: SocketAddr,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _task_manager: TaskManager,
}

impl NodeContext {
    pub async fn send_payload(&self, payload: Flashblock) -> eyre::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((payload, tx)).await?;
        rx.await?;
        Ok(())
    }

    pub async fn provider(&self) -> eyre::Result<RootProvider<Optimism>> {
        let url = format!("http://{}", self.http_api_addr);
        let client = RpcClient::builder().http(url.parse()?);
        Ok(RootProvider::<Optimism>::new(client))
    }
}

static INIT_TRACING: Once = Once::new();

fn init_logging_once() {
    INIT_TRACING.call_once(|| {
        reth_tracing::init_test_tracing();
    });
}

pub async fn setup_node() -> eyre::Result<NodeContext> {
    init_logging_once();
    let tasks = TaskManager::current();
    let exec = tasks.executor();
    const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

    let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))
        .expect("Genesis object can't be constructed from file ../assets/genesis.json");
    let chain_spec = Arc::new(
        OpChainSpecBuilder::base_mainnet()
            .genesis(genesis)
            .ecotone_activated()
            .chain(Chain::from(BASE_SEPOLIA_CHAIN_ID))
            .build(),
    );

    let network_config = NetworkArgs {
        discovery: DiscoveryArgs {
            disable_discovery: true,
            ..DiscoveryArgs::default()
        },
        ..NetworkArgs::default()
    };

    let node_config = NodeConfig::new(chain_spec.clone())
        .with_network(network_config.clone())
        .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
        .with_unused_ports();

    let node = OpNode::new(RollupArgs::default());

    let (sender, mut receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);

    let NodeHandle {
        node,
        node_exit_future,
    } = NodeBuilder::new(node_config.clone())
        .testing_node(exec.clone())
        .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
        .with_components(node.components_builder())
        .with_add_ons(node.add_ons())
        .extend_rpc_modules(move |ctx| {
            let flashblocks_state = Arc::new(FlashblocksState::new(ctx.provider().clone()));
            flashblocks_state.start();

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                flashblocks_state.clone(),
            );

            ctx.modules.replace_configured(api_ext.into_rpc())?;

            tokio::spawn(async move {
                while let Some((payload, tx)) = receiver.recv().await {
                    flashblocks_state.on_flashblock_received(payload);
                    tx.send(()).unwrap();
                }
            });

            Ok(())
        })
        .launch()
        .await?;

    let http_api_addr = node
        .rpc_server_handle()
        .http_local_addr()
        .ok_or_else(|| eyre::eyre!("Failed to get http api address"))?;

    Ok(NodeContext {
        sender,
        http_api_addr,
        _node_exit_future: node_exit_future,
        _node: Box::new(node),
        _task_manager: tasks,
    })
}

pub fn create_base_flashblock(block_number: u64, transactions: Vec<Bytes>) -> Flashblock {
    let mut receipts = HashMap::default();
    receipts.insert(
        BLOCK_INFO_TXN_HASH,
        OpReceipt::Deposit(OpDepositReceipt {
            inner: Receipt {
                status: true.into(),
                cumulative_gas_used: 21000,
                logs: vec![],
            },
            deposit_nonce: Some(4012992u64),
            deposit_receipt_version: None,
        }),
    );

    Flashblock {
        payload_id: PayloadId::new([0; 8]),
        index: block_number,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: B256::default(),
            parent_hash: B256::default(),
            fee_recipient: Address::ZERO,
            prev_randao: B256::default(),
            block_number,
            gas_limit: 30_000_000,
            timestamp: block_number,
            extra_data: Bytes::new(),
            base_fee_per_gas: alloy_primitives::U256::ZERO,
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            transactions: {
                let mut txs = vec![BLOCK_INFO_TXN];
                txs.extend(transactions);
                txs
            },
            ..Default::default()
        },
        metadata: Metadata {
            block_number,
            receipts,
            ..Default::default()
        },
    }
}
