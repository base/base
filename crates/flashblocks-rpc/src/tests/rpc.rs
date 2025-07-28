#[cfg(test)]
mod tests {
    use crate::rpc::{EthApiExt, EthApiOverrideServer};
    use crate::state::FlashblocksState;
    use crate::subscription::{Flashblock, Metadata};
    use alloy_consensus::Receipt;
    use alloy_genesis::Genesis;
    use alloy_primitives::map::HashMap;
    use alloy_primitives::{address, b256, Address, Bytes, TxHash, B256, U256};
    use alloy_provider::Provider;
    use alloy_provider::RootProvider;
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpDepositReceipt;
    use op_alloy_network::{Optimism, ReceiptResponse, TransactionResponse};
    use reth::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
    use reth::builder::{Node, NodeBuilder, NodeConfig, NodeHandle};
    use reth::core::exit::NodeExitFuture;
    use reth::tasks::TaskManager;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_node::args::RollupArgs;
    use reth_optimism_node::OpNode;
    use reth_optimism_primitives::OpReceipt;
    use reth_provider::providers::BlockchainProvider;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use serde_json;
    use std::any::Any;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};

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

        pub async fn send_test_payloads(&self) -> eyre::Result<()> {
            let base_payload = create_first_payload();
            self.send_payload(base_payload).await?;

            let second_payload = create_second_payload();
            self.send_payload(second_payload).await?;

            Ok(())
        }
    }

    async fn setup_node() -> eyre::Result<NodeContext> {
        let tasks = TaskManager::current();
        let exec = tasks.executor();

        let genesis: Genesis = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet()
                .genesis(genesis)
                .ecotone_activated()
                .build(),
        );

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs {
                disable_discovery: true,
                ..DiscoveryArgs::default()
            },
            ..NetworkArgs::default()
        };

        // Use with_unused_ports() to let Reth allocate random ports and avoid port collisions
        let node_config = NodeConfig::new(chain_spec.clone())
            .with_network(network_config.clone())
            .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
            .with_unused_ports();

        let node = OpNode::new(RollupArgs::default());

        // Start websocket server to simulate the builder and send payloads back to the node
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
                // We are not going to use the websocket connection to send payloads so we use
                // a dummy url.
                let flashblocks_state = Arc::new(FlashblocksState::new(chain_spec.clone(), 2000));

                let api_ext =
                    EthApiExt::new(ctx.registry.eth_api().clone(), flashblocks_state.clone(), 1);

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

    fn create_first_payload() -> Flashblock {
        let block_info_tx = Bytes::from_str(
            "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000")
            .unwrap();

        let block_info_txn_hash =
            B256::from_str("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6")
                .unwrap();

        Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number: 1,
                gas_limit: 0,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![block_info_tx],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        block_info_txn_hash,
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: Receipt {
                                status: true.into(),
                                cumulative_gas_used: 10000,
                                logs: vec![],
                            },
                            deposit_nonce: Some(4012991u64),
                            deposit_receipt_version: None,
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
            },
        }
    }

    fn create_second_payload() -> Flashblock {
        // Create second payload (index 1) with transactions
        let tx1 = Bytes::from_str("0x02f9041783014a348301d48d8459682f008459682f6a830493e09400a739e4479c97289801654ec1a52a67077613c080b903a437946af700000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000008ac7230489e8000000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000365746800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008ac7230489e8000000000000000000000000000000000000000000000000000000000000000001a0000000000000000000000000000000000000000000000000000000000000022000000000000000000000000000000000000000000000000000000000000002400000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000077365706f6c696100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002a30783438656438353962326366306339623662616338643731346531623634363132643132323464366400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c001a0566779287abe153521da0266841d92ba4ee6aebfa8dc4ddb8103237312405f13a022ecbaf20da5401e4229dba4afadb22434be624684ce8e5d62aa04e2f638f631").unwrap();
        let tx2 = Bytes::from_str("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14").unwrap();

        let payload = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                transactions: vec![tx1, tx2],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        TX1_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 31000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        TX2_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 55000,
                            logs: vec![],
                        }),
                    );
                    receipts
                },
                new_account_balances: {
                    let mut map = HashMap::default();
                    map.insert(TEST_ADDRESS, U256::from(PENDING_BALANCE));
                    map
                },
            },
        };

        payload
    }

    const TEST_ADDRESS: Address = address!("0x1234567890123456789012345678901234567890");
    const PENDING_BALANCE: u64 = 4660;

    const TX1_SENDER: Address = address!("0x48ed859B2Cf0c9b6bac8d714E1b64612D1224D6d");
    const TX2_SENDER: Address = address!("0x6e5e56b972374e4fde8390df0033397df931a49d");

    const TX1_HASH: TxHash =
        b256!("0x10b758f54be7d521724a8bd1bbac1da097cf3f552f6a105bb11bc569b8dd3b91");
    const TX2_HASH: TxHash =
        b256!("0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8");

    #[tokio::test]
    async fn test_get_pending_block() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        let latest_block = provider
            .get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)
            .await?
            .expect("latest block expected");
        assert_eq!(latest_block.number(), 0);

        // Querying pending block when it does not exists yet
        let pending_block = provider
            .get_block_by_number(alloy_eips::BlockNumberOrTag::Pending)
            .await?;
        assert_eq!(pending_block.is_none(), true);

        let base_payload = create_first_payload();
        node.send_payload(base_payload).await?;

        // Query pending block after sending the base payload with an empty delta
        let pending_block = provider
            .get_block_by_number(alloy_eips::BlockNumberOrTag::Pending)
            .await?
            .expect("pending block expected");

        assert_eq!(pending_block.number(), 1);
        assert_eq!(pending_block.transactions.hashes().len(), 1); // L1Info transaction

        let second_payload = create_second_payload();
        node.send_payload(second_payload).await?;

        // Query pending block after sending the second payload with two transactions
        let block = provider
            .get_block_by_number(alloy_eips::BlockNumberOrTag::Pending)
            .await?
            .expect("pending block expected");

        assert_eq!(block.number(), 1);
        assert_eq!(block.transactions.hashes().len(), 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_balance_pending() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        node.send_test_payloads().await?;

        let balance = provider.get_balance(TEST_ADDRESS).await?;
        assert_eq!(balance, U256::ZERO);

        let pending_balance = provider.get_balance(TEST_ADDRESS).pending().await?;
        assert_eq!(pending_balance, U256::from(PENDING_BALANCE));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_transaction_by_hash_pending() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        assert!(provider.get_transaction_by_hash(TX1_HASH).await?.is_none());
        assert!(provider.get_transaction_by_hash(TX2_HASH).await?.is_none());

        node.send_test_payloads().await?;

        let tx1 = provider
            .get_transaction_by_hash(TX1_HASH)
            .await?
            .expect("tx1 expected");
        assert_eq!(tx1.tx_hash(), TX1_HASH);
        assert_eq!(tx1.from(), TX1_SENDER);

        let tx2 = provider
            .get_transaction_by_hash(TX2_HASH)
            .await?
            .expect("tx2 expected");
        assert_eq!(tx2.tx_hash(), TX2_HASH);
        assert_eq!(tx2.from(), TX2_SENDER);

        // TODO: Verify more properties of the txns here.

        Ok(())
    }

    #[tokio::test]
    async fn test_get_transaction_receipt_pending() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        let receipt = provider.get_transaction_receipt(TX1_HASH).await?;
        assert_eq!(receipt.is_none(), true);

        node.send_test_payloads().await?;

        let receipt = provider
            .get_transaction_receipt(TX1_HASH)
            .await?
            .expect("receipt expected");
        assert_eq!(receipt.gas_used(), 21000);

        let receipt = provider
            .get_transaction_receipt(TX2_HASH)
            .await?
            .expect("receipt expected");
        assert_eq!(receipt.gas_used(), 24000); // 45000 - 21000

        // TODO: Add a new payload and validate that the receipts from the previous payload
        // are not returned.

        Ok(())
    }

    #[tokio::test]
    async fn test_get_transaction_count() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        assert_eq!(provider.get_transaction_count(TX1_SENDER).await?, 0);
        assert_eq!(
            provider.get_transaction_count(TX2_SENDER).pending().await?,
            0
        );

        node.send_test_payloads().await?;

        assert_eq!(provider.get_transaction_count(TX1_SENDER).await?, 0);
        assert_eq!(
            provider.get_transaction_count(TX2_SENDER).pending().await?,
            1
        );

        Ok(())
    }
}
