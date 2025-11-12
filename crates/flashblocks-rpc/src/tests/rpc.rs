#[cfg(test)]
mod tests {
    use crate::rpc::{EthApiExt, EthApiOverrideServer};
    use crate::state::FlashblocksState;
    use crate::subscription::{Flashblock, FlashblocksReceiver, Metadata};
    use crate::tests::{BLOCK_INFO_TXN, BLOCK_INFO_TXN_HASH};
    use alloy_consensus::Receipt;
    use alloy_eips::BlockNumberOrTag;
    use alloy_genesis::Genesis;
    use alloy_primitives::map::HashMap;
    use alloy_primitives::{Address, B256, Bytes, LogData, TxHash, U256, address, b256, bytes};
    use alloy_provider::Provider;
    use alloy_provider::RootProvider;
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types::simulate::{SimBlock, SimulatePayload};
    use alloy_rpc_types_engine::PayloadId;
    use alloy_rpc_types_eth::TransactionInput;
    use alloy_rpc_types_eth::error::EthRpcErrorCode;
    use op_alloy_consensus::OpDepositReceipt;
    use op_alloy_network::{Optimism, ReceiptResponse, TransactionResponse};
    use op_alloy_rpc_types::OpTransactionRequest;
    use reth::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
    use reth::builder::{Node, NodeBuilder, NodeConfig, NodeHandle};
    use reth::chainspec::Chain;
    use reth::core::exit::NodeExitFuture;
    use reth::tasks::TaskManager;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_node::OpNode;
    use reth_optimism_node::args::RollupArgs;
    use reth_optimism_primitives::OpReceipt;
    use reth_provider::providers::BlockchainProvider;
    use reth_rpc_eth_api::RpcReceipt;
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

        pub async fn send_raw_transaction_sync(
            &self,
            tx: Bytes,
            timeout_ms: Option<u64>,
        ) -> eyre::Result<RpcReceipt<Optimism>> {
            let url = format!("http://{}", self.http_api_addr);
            let client = RpcClient::new_http(url.parse()?);

            let receipt = client
                .request::<_, RpcReceipt<Optimism>>("eth_sendRawTransactionSync", (tx, timeout_ms))
                .await?;

            Ok(receipt)
        }
    }

    async fn setup_node() -> eyre::Result<NodeContext> {
        let tasks = TaskManager::current();
        let exec = tasks.executor();
        const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

        let genesis: Genesis = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
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

    fn create_first_payload() -> Flashblock {
        Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![BLOCK_INFO_TXN],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        BLOCK_INFO_TXN_HASH,
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

    const TEST_ADDRESS: Address = address!("0x1234567890123456789012345678901234567890");
    const PENDING_BALANCE: u64 = 4660;

    const DEPOSIT_SENDER: Address = address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001");
    const TX_SENDER: Address = address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");

    const DEPOSIT_TX_HASH: TxHash =
        b256!("0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548");
    const TRANSFER_ETH_HASH: TxHash =
        b256!("0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a");

    const DEPLOYMENT_HASH: TxHash =
        b256!("0x2b14d58c13406f25a78cfb802fb711c0d2c27bf9eccaec2d1847dc4392918f63");

    const INCREMENT_HASH: TxHash =
        b256!("0x993ad6a332752f6748636ce899b3791e4a33f7eece82c0db4556c7339c1b2929");
    const INCREMENT2_HASH: TxHash =
        b256!("0x617a3673399647d12bb82ec8eba2ca3fc468e99894bcf1c67eb50ef38ee615cb");

    const COUNTER_ADDRESS: Address = address!("0xe7f1725e7734ce288f8367e1bb143e90bb3f0512");

    // Test log topics - these represent common events
    const TEST_LOG_TOPIC_0: B256 =
        b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"); // Transfer event
    const TEST_LOG_TOPIC_1: B256 =
        b256!("0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"); // From address
    const TEST_LOG_TOPIC_2: B256 =
        b256!("0x0000000000000000000000001234567890123456789012345678901234567890"); // To address

    fn create_test_logs() -> Vec<alloy_primitives::Log> {
        vec![
            alloy_primitives::Log {
                address: COUNTER_ADDRESS,
                data: LogData::new(
                    vec![TEST_LOG_TOPIC_0, TEST_LOG_TOPIC_1, TEST_LOG_TOPIC_2],
                    bytes!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")
                        .into(), // 1 ETH in wei
                )
                .unwrap(),
            },
            alloy_primitives::Log {
                address: TEST_ADDRESS,
                data: LogData::new(
                    vec![TEST_LOG_TOPIC_0],
                    bytes!("0x0000000000000000000000000000000000000000000000000000000000000001")
                        .into(), // Value: 1
                )
                .unwrap(),
            },
        ]
    }

    // NOTE:
    // To create tx use cast mktx/
    // Example: `cast mktx --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --nonce 1 --gas-limit 100000 --gas-price 1499576 --chain 84532 --value 0 --priority-gas-price 0 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 0x`
    // Create second payload (index 1) with transactions
    // tx1 hash: 0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548 (deposit transaction)
    // tx2 hash: 0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a
    const DEPOSIT_TX: Bytes = bytes!(
        "0x7ef8f8a042a8ae5ec231af3d0f90f68543ec8bca1da4f7edd712d5b51b490688355a6db794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000044d000a118b00000000000000040000000067cb7cb0000000000077dbd4000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000014edd27304108914dd6503b19b9eeb9956982ef197febbeeed8a9eac3dbaaabdf000000000000000000000000fc56e7272eebbba5bc6c544e159483c4a38f8ba3"
    );
    const TRANSFER_ETH_TX: Bytes = bytes!(
        "0x02f87383014a3480808449504f80830186a094deaddeaddeaddeaddeaddeaddeaddeaddead00018ad3c21bcb3f6efc39800080c0019f5a6fe2065583f4f3730e82e5725f651cbbaf11dc1f82c8d29ba1f3f99e5383a061e0bf5dfff4a9bc521ad426eee593d3653c5c330ae8a65fad3175d30f291d31"
    );

    // NOTE:
    // Following txns deploy a double Counter contract (Compiled with solc 0.8.13)
    // contains a `uint256 public count = 1` and a function increment() { count++ };
    // and a `uint256 public count2 = 1` and a function increment2() { count2++ };
    // Following txn calls increment once, so count should be 2
    // Raw Bytecode: 0x608060405260015f55600180553480156016575f80fd5b50610218806100245f395ff3fe608060405234801561000f575f80fd5b5060043610610060575f3560e01c80631d63e24d146100645780637477f70014610082578063a87d942c146100a0578063ab57b128146100be578063d09de08a146100c8578063d631c639146100d2575b5f80fd5b61006c6100f0565b6040516100799190610155565b60405180910390f35b61008a6100f6565b6040516100979190610155565b60405180910390f35b6100a86100fb565b6040516100b59190610155565b60405180910390f35b6100c6610103565b005b6100d061011c565b005b6100da610134565b6040516100e79190610155565b60405180910390f35b60015481565b5f5481565b5f8054905090565b60015f8154809291906101159061019b565b9190505550565b5f8081548092919061012d9061019b565b9190505550565b5f600154905090565b5f819050919050565b61014f8161013d565b82525050565b5f6020820190506101685f830184610146565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f6101a58261013d565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff82036101d7576101d661016e565b5b60018201905091905056fea264697066735822122025c7e02ddf460dece9c1e52a3f9ff042055b58005168e7825d7f6c426288c27164736f6c63430008190033
    const DEPLOYMENT_TX: Bytes = bytes!(
        "0x02f9029483014a3401808449504f80830493e08080b9023c608060405260015f55600180553480156016575f80fd5b50610218806100245f395ff3fe608060405234801561000f575f80fd5b5060043610610060575f3560e01c80631d63e24d146100645780637477f70014610082578063a87d942c146100a0578063ab57b128146100be578063d09de08a146100c8578063d631c639146100d2575b5f80fd5b61006c6100f0565b6040516100799190610155565b60405180910390f35b61008a6100f6565b6040516100979190610155565b60405180910390f35b6100a86100fb565b6040516100b59190610155565b60405180910390f35b6100c6610103565b005b6100d061011c565b005b6100da610134565b6040516100e79190610155565b60405180910390f35b60015481565b5f5481565b5f8054905090565b60015f8154809291906101159061019b565b9190505550565b5f8081548092919061012d9061019b565b9190505550565b5f600154905090565b5f819050919050565b61014f8161013d565b82525050565b5f6020820190506101685f830184610146565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f6101a58261013d565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff82036101d7576101d661016e565b5b60018201905091905056fea264697066735822122025c7e02ddf460dece9c1e52a3f9ff042055b58005168e7825d7f6c426288c27164736f6c63430008190033c001a02f196658032e0b003bcd234349d63081f5d6c2785264c6fec6b25ad877ae326aa0290c9f96f4501439b07a7b5e8e938f15fc30a9c15db3fc5e654d44e1f522060c"
    );
    // Increment tx: call increment()
    const INCREMENT_TX: Bytes = bytes!(
        "0x02f86d83014a3402808449504f8082abe094e7f1725e7734ce288f8367e1bb143e90bb3f05128084d09de08ac080a0a9c1a565668084d4052bbd9bc3abce8555a06aed6651c82c2756ac8a83a79fa2a03427f440ce4910a5227ea0cedb60b06cf0bea2dbbac93bd37efa91a474c29d89"
    );
    // Increment2 tx: call increment2()
    const INCREMENT2_TX: Bytes = bytes!(
        "0x02f86d83014a3403808449504f8082abe094e7f1725e7734ce288f8367e1bb143e90bb3f05128084ab57b128c001a03a155b8c81165fc8193aa739522c2a9e432e274adea7f0b90ef2b5078737f153a0288d7fad4a3b0d1e7eaf7fab63b298393a5020bf11d91ff8df13b235410799e2"
    );

    fn create_second_payload() -> Flashblock {
        let payload = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                transactions: vec![
                    DEPOSIT_TX,
                    TRANSFER_ETH_TX,
                    DEPLOYMENT_TX,
                    INCREMENT_TX,
                    INCREMENT2_TX,
                ],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        DEPOSIT_TX_HASH,
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: Receipt {
                                status: true.into(),
                                cumulative_gas_used: 31000,
                                logs: vec![],
                            },
                            deposit_nonce: Some(4012992u64),
                            deposit_receipt_version: None,
                        }),
                    );
                    receipts.insert(
                        TRANSFER_ETH_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 55000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        DEPLOYMENT_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 272279,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        INCREMENT_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 272279 + 44000,
                            logs: create_test_logs(),
                        }),
                    );
                    receipts.insert(
                        INCREMENT2_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 272279 + 44000 + 44000,
                            logs: vec![],
                        }),
                    );
                    receipts
                },
                new_account_balances: {
                    let mut map = HashMap::default();
                    map.insert(TEST_ADDRESS, U256::from(PENDING_BALANCE));
                    map.insert(COUNTER_ADDRESS, U256::from(0));
                    map
                },
            },
        };

        payload
    }

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

        // Querying pending block when it does not exist yet
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
        assert_eq!(block.transactions.hashes().len(), 6);

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

        assert!(
            provider
                .get_transaction_by_hash(DEPOSIT_TX_HASH)
                .await?
                .is_none()
        );
        assert!(
            provider
                .get_transaction_by_hash(TRANSFER_ETH_HASH)
                .await?
                .is_none()
        );

        node.send_test_payloads().await?;

        let tx1 = provider
            .get_transaction_by_hash(DEPOSIT_TX_HASH)
            .await?
            .expect("tx1 expected");
        assert_eq!(tx1.tx_hash(), DEPOSIT_TX_HASH);
        assert_eq!(tx1.from(), DEPOSIT_SENDER);

        let tx2 = provider
            .get_transaction_by_hash(TRANSFER_ETH_HASH)
            .await?
            .expect("tx2 expected");
        assert_eq!(tx2.tx_hash(), TRANSFER_ETH_HASH);
        assert_eq!(tx2.from(), TX_SENDER);

        // Verify additional transaction properties
        assert_eq!(tx1.nonce(), 0, "deposit transaction should have nonce 0");
        assert_eq!(
            tx1.to(),
            Some(address!("0x4200000000000000000000000000000000000015")),
            "deposit transaction recipient mismatch"
        );

        assert_eq!(tx2.nonce(), 0, "transfer transaction should have nonce 0");
        assert_eq!(
            tx2.to(),
            Some(DEPOSIT_SENDER),
            "transfer transaction recipient mismatch"
        );
        assert_eq!(
            tx2.gas_limit(),
            100000,
            "transfer transaction gas limit mismatch"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_transaction_receipt_pending() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        let receipt = provider.get_transaction_receipt(DEPOSIT_TX_HASH).await?;
        assert_eq!(receipt.is_none(), true);

        node.send_test_payloads().await?;

        let receipt = provider
            .get_transaction_receipt(DEPOSIT_TX_HASH)
            .await?
            .expect("receipt expected");
        assert_eq!(receipt.gas_used(), 21000);

        let receipt = provider
            .get_transaction_receipt(TRANSFER_ETH_HASH)
            .await?
            .expect("receipt expected");
        assert_eq!(receipt.gas_used(), 24000); // 45000 - 21000

        // Verify that when a new payload is sent, receipts from the previous payload
        // are replaced and old transaction receipts are no longer available
        let third_payload = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 2,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                transactions: vec![DEPLOYMENT_TX],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        DEPLOYMENT_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 217279,
                            logs: vec![],
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
            },
        };
        node.send_payload(third_payload).await?;

        // Previous payload's receipts should no longer be available
        assert!(
            provider
                .get_transaction_receipt(DEPOSIT_TX_HASH)
                .await?
                .is_none(),
            "deposit receipt from previous payload should not be returned"
        );
        assert!(
            provider
                .get_transaction_receipt(TRANSFER_ETH_HASH)
                .await?
                .is_none(),
            "transfer receipt from previous payload should not be returned"
        );

        // New payload's receipt should be available
        let new_receipt = provider
            .get_transaction_receipt(DEPLOYMENT_HASH)
            .await?
            .expect("new deployment receipt should be available");
        assert_eq!(
            new_receipt.gas_used(),
            217279,
            "new deployment receipt gas_used mismatch"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_transaction_count() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).await?, 0);
        assert_eq!(
            provider.get_transaction_count(TX_SENDER).pending().await?,
            0
        );

        node.send_test_payloads().await?;

        assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).await?, 0);
        assert_eq!(
            provider.get_transaction_count(TX_SENDER).pending().await?,
            4
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eth_call() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;

        let provider = node.provider().await?;

        // We ensure that eth_call will succeed because we are on plain state
        let send_eth_call = OpTransactionRequest::default()
            .from(TX_SENDER)
            .transaction_type(0)
            .gas_limit(200000)
            .nonce(1)
            .to(address!("0xf39635f2adf40608255779ff742afe13de31f577"))
            .value(U256::from(9999999999849942300000u128))
            .input(TransactionInput::new(bytes!("0x")));

        let res = provider
            .call(send_eth_call.clone())
            .block(BlockNumberOrTag::Pending.into())
            .await;

        assert!(res.is_ok());

        node.send_test_payloads().await?;

        // We included a heavy spending transaction and now don't have enough funds for this request, so
        // this eth_call with fail
        let res = provider
            .call(send_eth_call.nonce(4))
            .block(BlockNumberOrTag::Pending.into())
            .await;

        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .as_error_resp()
                .unwrap()
                .message
                .contains("insufficient funds for gas")
        );

        // read count1 from counter contract
        let eth_call_count1 = OpTransactionRequest::default()
            .from(TX_SENDER)
            .transaction_type(0)
            .gas_limit(20000000)
            .nonce(5)
            .to(COUNTER_ADDRESS)
            .value(U256::ZERO)
            .input(TransactionInput::new(bytes!("0xa87d942c")));
        let res_count1 = provider.call(eth_call_count1).await;
        assert!(res_count1.is_ok());
        assert_eq!(
            U256::from_str(res_count1.unwrap().to_string().as_str()).unwrap(),
            U256::from(2)
        );

        // read count2 from counter contract
        let eth_call_count2 = OpTransactionRequest::default()
            .from(TX_SENDER)
            .transaction_type(0)
            .gas_limit(20000000)
            .nonce(6)
            .to(COUNTER_ADDRESS)
            .value(U256::ZERO)
            .input(TransactionInput::new(bytes!("0xd631c639")));
        let res_count2 = provider.call(eth_call_count2).await;
        assert!(res_count2.is_ok());
        assert_eq!(
            U256::from_str(res_count2.unwrap().to_string().as_str()).unwrap(),
            U256::from(2)
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eth_estimate_gas() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;

        let provider = node.provider().await?;

        // We ensure that eth_estimate_gas will succeed because we are on plain state
        let send_estimate_gas = OpTransactionRequest::default()
            .from(TX_SENDER)
            .transaction_type(0)
            .gas_limit(200000)
            .nonce(1)
            .to(address!("0xf39635f2adf40608255779ff742afe13de31f577"))
            .value(U256::from(9999999999849942300000u128))
            .input(TransactionInput::new(bytes!("0x")));

        let res = provider
            .estimate_gas(send_estimate_gas.clone())
            .block(BlockNumberOrTag::Pending.into())
            .await;

        assert!(res.is_ok());

        node.send_test_payloads().await?;

        // We included a heavy spending transaction and now don't have enough funds for this request, so
        // this eth_estimate_gas with fail
        let res = provider
            .estimate_gas(send_estimate_gas.nonce(4))
            .block(BlockNumberOrTag::Pending.into())
            .await;

        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .as_error_resp()
                .unwrap()
                .message
                .contains("insufficient funds for gas")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eth_simulate_v1() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;
        node.send_test_payloads().await?;

        let simulate_call = SimulatePayload {
            block_state_calls: vec![SimBlock {
                calls: vec![
                    // read number from counter contract
                    OpTransactionRequest::default()
                        .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
                        .transaction_type(0)
                        .gas_limit(200000)
                        .to(address!("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"))
                        .value(U256::ZERO)
                        .input(TransactionInput::new(bytes!("0xa87d942c")))
                        .into(),
                    // increment() value in contract
                    OpTransactionRequest::default()
                        .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
                        .transaction_type(0)
                        .gas_limit(200000)
                        .to(address!("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"))
                        .input(TransactionInput::new(bytes!("0xd09de08a")))
                        .into(),
                    // read number from counter contract
                    OpTransactionRequest::default()
                        .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
                        .transaction_type(0)
                        .gas_limit(200000)
                        .to(address!("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"))
                        .value(U256::ZERO)
                        .input(TransactionInput::new(bytes!("0xa87d942c")))
                        .into(),
                ],
                block_overrides: None,
                state_overrides: None,
            }],
            trace_transfers: false,
            validation: true,
            return_full_transactions: true,
        };
        let simulate_res = provider
            .simulate(&simulate_call)
            .block_id(BlockNumberOrTag::Pending.into())
            .await;
        assert!(simulate_res.is_ok());
        let block = simulate_res.unwrap();
        assert_eq!(block.len(), 1);
        assert_eq!(block[0].calls.len(), 3);
        assert_eq!(
            block[0].calls[0].return_data,
            bytes!("0x0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(block[0].calls[1].return_data, bytes!("0x"));
        assert_eq!(
            block[0].calls[2].return_data,
            bytes!("0x0000000000000000000000000000000000000000000000000000000000000003")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_send_raw_transaction_sync() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;

        node.send_payload(create_first_payload()).await?;

        // run the Tx sync and, in parallel, deliver the payload that contains the Tx
        let (receipt_result, payload_result) = tokio::join!(
            node.send_raw_transaction_sync(TRANSFER_ETH_TX, None),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                node.send_payload(create_second_payload()).await
            }
        );

        payload_result?;
        let receipt = receipt_result?;

        assert_eq!(receipt.transaction_hash(), TRANSFER_ETH_HASH);
        Ok(())
    }

    #[tokio::test]
    async fn test_send_raw_transaction_sync_timeout() {
        reth_tracing::init_test_tracing();
        let node = setup_node().await.unwrap();

        // fail request immediately by passing a timeout of 0 ms
        let receipt_result = node
            .send_raw_transaction_sync(TRANSFER_ETH_TX, Some(0))
            .await;

        let error_code = EthRpcErrorCode::TransactionConfirmationTimeout.code();
        assert!(
            receipt_result
                .err()
                .unwrap()
                .to_string()
                .contains(format!("{}", error_code).as_str())
        );
    }

    #[tokio::test]
    async fn test_get_logs_pending() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        // Test no logs when no flashblocks sent
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .select(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;
        assert_eq!(logs.len(), 0);

        // Send payloads with transactions
        node.send_test_payloads().await?;

        // Test getting pending logs - must use both fromBlock and toBlock as "pending"
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .from_block(alloy_eips::BlockNumberOrTag::Pending)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // We should now have 2 logs from the INCREMENT_TX transaction
        assert_eq!(logs.len(), 2);

        // Verify the first log is from COUNTER_ADDRESS
        assert_eq!(logs[0].address(), COUNTER_ADDRESS);
        assert_eq!(logs[0].topics()[0], TEST_LOG_TOPIC_0);
        assert_eq!(logs[0].transaction_hash, Some(INCREMENT_HASH));

        // Verify the second log is from TEST_ADDRESS
        assert_eq!(logs[1].address(), TEST_ADDRESS);
        assert_eq!(logs[1].topics()[0], TEST_LOG_TOPIC_0);
        assert_eq!(logs[1].transaction_hash, Some(INCREMENT_HASH));

        Ok(())
    }

    #[tokio::test]
    async fn test_get_logs_filter_by_address() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        node.send_test_payloads().await?;

        // Test filtering by a specific address (COUNTER_ADDRESS)
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .address(COUNTER_ADDRESS)
                    .from_block(alloy_eips::BlockNumberOrTag::Pending)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // Should get only 1 log from COUNTER_ADDRESS
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address(), COUNTER_ADDRESS);
        assert_eq!(logs[0].transaction_hash, Some(INCREMENT_HASH));

        // Test filtering by TEST_ADDRESS
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .address(TEST_ADDRESS)
                    .from_block(alloy_eips::BlockNumberOrTag::Pending)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // Should get only 1 log from TEST_ADDRESS
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address(), TEST_ADDRESS);
        assert_eq!(logs[0].transaction_hash, Some(INCREMENT_HASH));

        Ok(())
    }

    #[tokio::test]
    async fn test_get_logs_topic_filtering() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        node.send_test_payloads().await?;

        // Test filtering by topic - should match both logs
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .event_signature(TEST_LOG_TOPIC_0)
                    .from_block(alloy_eips::BlockNumberOrTag::Pending)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|log| log.topics()[0] == TEST_LOG_TOPIC_0));

        // Test filtering by specific topic combination - should match only the first log
        let filter = alloy_rpc_types_eth::Filter::default()
            .topic1(TEST_LOG_TOPIC_1)
            .from_block(alloy_eips::BlockNumberOrTag::Pending)
            .to_block(alloy_eips::BlockNumberOrTag::Pending);

        let logs = provider.get_logs(&filter).await?;

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address(), COUNTER_ADDRESS);
        assert_eq!(logs[0].topics()[1], TEST_LOG_TOPIC_1);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_logs_mixed_block_ranges() -> eyre::Result<()> {
        reth_tracing::init_test_tracing();
        let node = setup_node().await?;
        let provider = node.provider().await?;

        node.send_test_payloads().await?;

        // Test fromBlock: 0, toBlock: pending (should include both historical and pending)
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .from_block(0)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // Should now include pending logs (2 logs from our test setup)
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .all(|log| log.transaction_hash == Some(INCREMENT_HASH))
        );

        // Test fromBlock: latest, toBlock: pending
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .from_block(alloy_eips::BlockNumberOrTag::Latest)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // Should include pending logs (historical part is empty in our test setup)
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .all(|log| log.transaction_hash == Some(INCREMENT_HASH))
        );

        // Test fromBlock: earliest, toBlock: pending
        let logs = provider
            .get_logs(
                &alloy_rpc_types_eth::Filter::default()
                    .from_block(alloy_eips::BlockNumberOrTag::Earliest)
                    .to_block(alloy_eips::BlockNumberOrTag::Pending),
            )
            .await?;

        // Should include pending logs (historical part is empty in our test setup)
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .all(|log| log.transaction_hash == Some(INCREMENT_HASH))
        );

        Ok(())
    }
}
