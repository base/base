//! Integration tests covering the Flashblocks RPC surface area.

use std::str::FromStr;

use DoubleCounter::DoubleCounterInstance;
use alloy_consensus::{Receipt, Transaction};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{
    Address, B256, Bytes, LogData, TxHash, U256, address, b256, bytes, map::HashMap,
};
use alloy_provider::Provider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types::simulate::{SimBlock, SimulatePayload};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::{TransactionInput, error::EthRpcErrorCode};
use base_reth_flashblocks::{Flashblock, Metadata};
use base_reth_test_utils::{
    DoubleCounter, FlashblocksHarness, L1_BLOCK_INFO_DEPOSIT_TX, L1_BLOCK_INFO_DEPOSIT_TX_HASH,
};
use eyre::Result;
use futures_util::{SinkExt, StreamExt};
use op_alloy_consensus::OpDepositReceipt;
use op_alloy_network::{Optimism, ReceiptResponse, TransactionResponse};
use op_alloy_rpc_types::OpTransactionRequest;
use reth::revm::context::TransactionType;
use reth_optimism_primitives::OpReceipt;
use reth_rpc_eth_api::RpcReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct TestSetup {
    harness: FlashblocksHarness,
    txn_details: TransactionDetails,
}

struct TransactionDetails {
    counter_deployment_tx: Bytes,
    counter_deployment_hash: TxHash,
    counter_address: Address,

    counter_increment_tx: Bytes,
    counter_increment_hash: TxHash,

    counter_increment2_tx: Bytes,
    counter_increment2_hash: TxHash,

    alice_eth_transfer_tx: Bytes,
    alice_eth_transfer_hash: TxHash,
}

impl TestSetup {
    async fn new() -> Result<Self> {
        let harness = FlashblocksHarness::new().await?;

        let provider = harness.provider();
        let deployer = &harness.accounts().deployer;
        let alice = &harness.accounts().alice;
        let bob = &harness.accounts().bob;

        let (counter_deployment_tx, counter_address, counter_deployment_hash) = deployer
            .create_deployment_tx(DoubleCounter::BYTECODE.clone(), 0)
            .expect("should be able to sign DoubleCounter deployment txn");
        let counter = DoubleCounterInstance::new(counter_address.clone(), provider);
        let (increment1_tx, increment1_tx_hash) = deployer
            .sign_txn_request(counter.increment().into_transaction_request().nonce(1))
            .expect("should be able to sign increment() txn");
        let (increment2_tx, increment2_tx_hash) = deployer
            .sign_txn_request(counter.increment2().into_transaction_request().nonce(2))
            .expect("should be able to sign increment2() txn");
        let (eth_transfer_tx, eth_transfer_hash) = alice
            .sign_txn_request(
                OpTransactionRequest::default()
                    .from(alice.address)
                    .transaction_type(TransactionType::Eip1559.into())
                    .gas_limit(100_000)
                    .nonce(0)
                    .to(bob.address)
                    .value(U256::from_str("999999999000000000000000").unwrap())
                    .into(),
            )
            .expect("should be able to sign eth transfer txn");

        let txn_details = TransactionDetails {
            counter_deployment_tx,
            counter_deployment_hash,
            counter_address,
            counter_increment_tx: increment1_tx,
            counter_increment_hash: increment1_tx_hash,
            counter_increment2_tx: increment2_tx,
            counter_increment2_hash: increment2_tx_hash,
            alice_eth_transfer_tx: eth_transfer_tx,
            alice_eth_transfer_hash: eth_transfer_hash,
        };

        Ok(Self { harness, txn_details })
    }

    fn create_first_payload(&self) -> Flashblock {
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
                blob_gas_used: Some(0),
                transactions: vec![L1_BLOCK_INFO_DEPOSIT_TX],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        L1_BLOCK_INFO_DEPOSIT_TX_HASH,
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

    fn create_second_payload(&self) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                blob_gas_used: Some(0),
                transactions: vec![
                    DEPOSIT_TX,
                    self.txn_details.alice_eth_transfer_tx.clone(),
                    self.txn_details.counter_deployment_tx.clone(),
                    self.txn_details.counter_increment_tx.clone(),
                    self.txn_details.counter_increment2_tx.clone(),
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
                        self.txn_details.alice_eth_transfer_hash,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 55000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        self.txn_details.counter_deployment_hash,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 272279,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        self.txn_details.counter_increment_hash,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 272279 + 44000,
                            logs:   vec![
                                    alloy_primitives::Log {
                                        address: self.txn_details.counter_address,
                                        data: LogData::new(
                                            vec![TEST_LOG_TOPIC_0, TEST_LOG_TOPIC_1, TEST_LOG_TOPIC_2],
                                            bytes!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000").into(), // 1 ETH in wei
                                        )
                                        .unwrap(),
                                    },
                                    alloy_primitives::Log {
                                        address: TEST_ADDRESS,
                                        data: LogData::new(
                                            vec![TEST_LOG_TOPIC_0],
                                            bytes!("0x0000000000000000000000000000000000000000000000000000000000000001").into(), // Value: 1
                                        )
                                        .unwrap(),
                                    },
                                ]
                        }),
                    );
                    receipts.insert(
                        self.txn_details.counter_increment2_hash,
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
                    map
                },
            },
        }
    }

    fn count1(&self) -> OpTransactionRequest {
        let counter =
            DoubleCounterInstance::new(self.txn_details.counter_address, self.harness.provider());
        counter.count1().into_transaction_request()
    }

    fn count2(&self) -> OpTransactionRequest {
        let counter =
            DoubleCounterInstance::new(self.txn_details.counter_address, self.harness.provider());
        counter.count2().into_transaction_request()
    }

    async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.harness.send_flashblock(flashblock).await
    }

    async fn send_test_payloads(&self) -> Result<()> {
        let base_payload = self.create_first_payload();
        self.send_flashblock(base_payload).await?;

        let second_payload = self.create_second_payload();
        self.send_flashblock(second_payload).await?;

        Ok(())
    }

    async fn send_raw_transaction_sync(
        &self,
        tx: Bytes,
        timeout_ms: Option<u64>,
    ) -> Result<RpcReceipt<Optimism>> {
        let url = self.harness.rpc_url();
        let client = RpcClient::new_http(url.parse()?);

        let receipt = client
            .request::<_, RpcReceipt<Optimism>>("eth_sendRawTransactionSync", (tx, timeout_ms))
            .await?;

        Ok(receipt)
    }
}

// Test constants
const TEST_ADDRESS: Address = address!("0x1234567890123456789012345678901234567890");
const PENDING_BALANCE: u64 = 4660;

const DEPOSIT_SENDER: Address = address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001");
const DEPOSIT_TX: Bytes = bytes!(
    "0x7ef8f8a042a8ae5ec231af3d0f90f68543ec8bca1da4f7edd712d5b51b490688355a6db794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000044d000a118b00000000000000040000000067cb7cb0000000000077dbd4000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000014edd27304108914dd6503b19b9eeb9956982ef197febbeeed8a9eac3dbaaabdf000000000000000000000000fc56e7272eebbba5bc6c544e159483c4a38f8ba3"
);
const DEPOSIT_TX_HASH: TxHash =
    b256!("0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548");

// Test log topics - these represent common events
const TEST_LOG_TOPIC_0: B256 =
    b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"); // Transfer event
const TEST_LOG_TOPIC_1: B256 =
    b256!("0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"); // From address
const TEST_LOG_TOPIC_2: B256 =
    b256!("0x0000000000000000000000001234567890123456789012345678901234567890"); // To address

#[tokio::test]
async fn test_get_pending_block() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let latest_block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .expect("latest block expected");
    assert_eq!(latest_block.number(), 0);

    // Querying pending block when it does not exist yet
    let pending_block = provider
        .get_block_by_number(BlockNumberOrTag::Pending)
        .await?
        .expect("latest block expected");

    assert_eq!(pending_block.number(), latest_block.number());
    assert_eq!(pending_block.hash(), latest_block.hash());

    let base_payload = setup.create_first_payload();
    setup.send_flashblock(base_payload).await?;

    // Query pending block after sending the base payload with an empty delta
    let pending_block = provider
        .get_block_by_number(BlockNumberOrTag::Pending)
        .await?
        .expect("pending block expected");

    assert_eq!(pending_block.number(), 1);
    assert_eq!(pending_block.transactions.hashes().len(), 1); // L1Info transaction

    let second_payload = setup.create_second_payload();
    setup.send_flashblock(second_payload).await?;

    // Query pending block after sending the second payload with two transactions
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Pending)
        .await?
        .expect("pending block expected");

    assert_eq!(block.number(), 1);
    assert_eq!(block.transactions.hashes().len(), 6);

    Ok(())
}

#[tokio::test]
async fn test_get_balance_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    setup.send_test_payloads().await?;

    let balance = provider.get_balance(TEST_ADDRESS).await?;
    assert_eq!(balance, U256::ZERO);

    let pending_balance = provider.get_balance(TEST_ADDRESS).pending().await?;
    assert_eq!(pending_balance, U256::from(PENDING_BALANCE));
    Ok(())
}

#[tokio::test]
async fn test_get_transaction_by_hash_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    assert!(provider.get_transaction_by_hash(DEPOSIT_TX_HASH).await?.is_none());
    assert!(
        provider
            .get_transaction_by_hash(setup.txn_details.alice_eth_transfer_hash)
            .await?
            .is_none()
    );

    setup.send_test_payloads().await?;

    let tx1 = provider.get_transaction_by_hash(DEPOSIT_TX_HASH).await?.expect("tx1 expected");
    assert_eq!(tx1.tx_hash(), DEPOSIT_TX_HASH);
    assert_eq!(tx1.from(), DEPOSIT_SENDER);

    let tx2 = provider
        .get_transaction_by_hash(setup.txn_details.alice_eth_transfer_hash)
        .await?
        .expect("tx2 expected");
    assert_eq!(tx2.tx_hash(), setup.txn_details.alice_eth_transfer_hash);
    assert_eq!(tx2.from(), setup.harness.accounts().alice.address);
    assert_eq!(
        tx2.inner.inner.as_eip1559().unwrap().to().unwrap(),
        setup.harness.accounts().bob.address
    );

    Ok(())
}

#[tokio::test]
async fn test_get_transaction_receipt_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let receipt = provider.get_transaction_receipt(DEPOSIT_TX_HASH).await?;
    assert_eq!(receipt.is_none(), true);

    setup.send_test_payloads().await?;

    let receipt =
        provider.get_transaction_receipt(DEPOSIT_TX_HASH).await?.expect("receipt expected");
    assert_eq!(receipt.gas_used(), 21000);

    let receipt = provider
        .get_transaction_receipt(setup.txn_details.alice_eth_transfer_hash)
        .await?
        .expect("receipt expected");
    assert_eq!(receipt.gas_used(), 24000); // 45000 - 21000

    Ok(())
}

#[tokio::test]
async fn test_get_transaction_count() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let deployer_addr = setup.harness.accounts().deployer.address;
    let alice_addr = setup.harness.accounts().alice.address;

    assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).pending().await?, 0);
    assert_eq!(provider.get_transaction_count(deployer_addr).pending().await?, 0);
    assert_eq!(provider.get_transaction_count(alice_addr).pending().await?, 0);

    setup.send_test_payloads().await?;

    assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).pending().await?, 2);
    assert_eq!(provider.get_transaction_count(deployer_addr).pending().await?, 3);
    assert_eq!(provider.get_transaction_count(alice_addr).pending().await?, 1);

    Ok(())
}

#[tokio::test]
async fn test_eth_call() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let accounts = setup.harness.accounts();

    // Initially, the big spend will succeed because we haven't sent the test payloads yet
    let big_spend = OpTransactionRequest::default()
        .from(accounts.alice.address)
        .transaction_type(0)
        .gas_limit(200000)
        .nonce(0)
        .to(setup.harness.accounts().bob.address)
        .value(U256::from(9999999999849942300000u128));

    let res = provider.call(big_spend.clone()).block(BlockNumberOrTag::Pending.into()).await;
    assert!(res.is_ok());

    setup.send_test_payloads().await?;

    // We included a big spending transaction in the payloads
    // and now don't have enough funds for this request, so this eth_call with fail
    let res =
        provider.call(big_spend.clone().nonce(3)).block(BlockNumberOrTag::Pending.into()).await;
    assert!(res.is_err());
    assert!(
        res.unwrap_err().as_error_resp().unwrap().message.contains("insufficient funds for gas")
    );

    // read count1 from counter contract
    let res_count1 = provider.call(setup.count1()).await;
    assert!(res_count1.is_ok());
    assert_eq!(U256::from_str(res_count1.unwrap().to_string().as_str()).unwrap(), U256::from(2));

    // read count2 from counter contract
    let res_count2 = provider.call(setup.count2()).await;
    assert!(res_count2.is_ok());
    assert_eq!(U256::from_str(res_count2.unwrap().to_string().as_str()).unwrap(), U256::from(2));

    Ok(())
}

#[tokio::test]
async fn test_eth_estimate_gas() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    // We ensure that eth_estimate_gas will succeed because we are on plain state
    let send_estimate_gas = OpTransactionRequest::default()
        .from(setup.harness.accounts().alice.address)
        .transaction_type(0)
        .gas_limit(200000)
        .nonce(0)
        .to(setup.harness.accounts().bob.address)
        .value(U256::from(9999999999849942300000u128))
        .input(TransactionInput::new(bytes!("0x")));

    let res = provider
        .estimate_gas(send_estimate_gas.clone())
        .block(BlockNumberOrTag::Pending.into())
        .await;

    assert!(res.is_ok());

    setup.send_test_payloads().await?;

    // We included a heavy spending transaction and now don't have enough funds for this request, so
    // this eth_estimate_gas with fail
    let res = provider
        .estimate_gas(send_estimate_gas.nonce(4))
        .block(BlockNumberOrTag::Pending.into())
        .await;

    assert!(res.is_err());
    assert!(
        res.unwrap_err().as_error_resp().unwrap().message.contains("insufficient funds for gas")
    );

    Ok(())
}

#[tokio::test]
async fn test_eth_simulate_v1() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();
    setup.send_test_payloads().await?;

    let simulate_call = SimulatePayload {
        block_state_calls: vec![SimBlock {
            calls: vec![
                // read count1() from counter contract
                setup.count1().into(),
                // increment() value in contract
                OpTransactionRequest::default()
                    .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
                    .transaction_type(0)
                    .gas_limit(200000)
                    .to(setup.txn_details.counter_address)
                    .input(TransactionInput::new(bytes!("0xd09de08a")))
                    .into(),
                // read count1() from counter contract
                setup.count1().into(),
            ],
            block_overrides: None,
            state_overrides: None,
        }],
        trace_transfers: false,
        validation: true,
        return_full_transactions: true,
    };
    let simulate_res =
        provider.simulate(&simulate_call).block_id(BlockNumberOrTag::Pending.into()).await;
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
async fn test_send_raw_transaction_sync() -> Result<()> {
    let setup = TestSetup::new().await?;

    setup.send_flashblock(setup.create_first_payload()).await?;

    // run the Tx sync and, in parallel, deliver the payload that contains the Tx
    let second_payload = setup.create_second_payload();
    let (receipt_result, payload_result) = tokio::join!(
        setup.send_raw_transaction_sync(setup.txn_details.alice_eth_transfer_tx.clone(), None),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            setup.send_flashblock(second_payload).await
        }
    );

    payload_result?;
    let receipt = receipt_result?;

    assert_eq!(receipt.transaction_hash(), setup.txn_details.alice_eth_transfer_hash);
    Ok(())
}

#[tokio::test]
async fn test_send_raw_transaction_sync_timeout() {
    let setup = TestSetup::new().await.unwrap();

    // fail request immediately by passing a timeout of 0 ms
    let receipt_result = setup
        .send_raw_transaction_sync(setup.txn_details.alice_eth_transfer_tx.clone(), Some(0))
        .await;

    let error_code = EthRpcErrorCode::TransactionConfirmationTimeout.code();
    assert!(receipt_result.err().unwrap().to_string().contains(format!("{}", error_code).as_str()));
}

#[tokio::test]
async fn test_get_logs_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    // Test no logs when no flashblocks sent
    let logs = provider
        .get_logs(
            &alloy_rpc_types_eth::Filter::default().select(alloy_eips::BlockNumberOrTag::Pending),
        )
        .await?;
    assert_eq!(logs.len(), 0);

    // Send payloads with transactions
    setup.send_test_payloads().await?;

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
    assert_eq!(logs[0].address(), setup.txn_details.counter_address);
    assert_eq!(logs[0].topics()[0], TEST_LOG_TOPIC_0);
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.counter_increment_hash));

    // Verify the second log is from TEST_ADDRESS
    assert_eq!(logs[1].address(), TEST_ADDRESS);
    assert_eq!(logs[1].topics()[0], TEST_LOG_TOPIC_0);
    assert_eq!(logs[1].transaction_hash, Some(setup.txn_details.counter_increment_hash));

    Ok(())
}

#[tokio::test]
async fn test_get_logs_filter_by_address() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    setup.send_test_payloads().await?;

    // Test filtering by a specific address (COUNTER_ADDRESS)
    let logs = provider
        .get_logs(
            &alloy_rpc_types_eth::Filter::default()
                .address(setup.txn_details.counter_address)
                .from_block(alloy_eips::BlockNumberOrTag::Pending)
                .to_block(alloy_eips::BlockNumberOrTag::Pending),
        )
        .await?;

    // Should get only 1 log from COUNTER_ADDRESS
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].address(), setup.txn_details.counter_address);
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.counter_increment_hash));

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
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.counter_increment_hash));

    Ok(())
}

#[tokio::test]
async fn test_get_logs_topic_filtering() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    setup.send_test_payloads().await?;

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
    assert_eq!(logs[0].address(), setup.txn_details.counter_address);
    assert_eq!(logs[0].topics()[1], TEST_LOG_TOPIC_1);

    Ok(())
}

#[tokio::test]
async fn test_get_logs_mixed_block_ranges() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    setup.send_test_payloads().await?;

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
            .all(|log| log.transaction_hash == Some(setup.txn_details.counter_increment_hash))
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
            .all(|log| log.transaction_hash == Some(setup.txn_details.counter_increment_hash))
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
            .all(|log| log.transaction_hash == Some(setup.txn_details.counter_increment_hash))
    );

    Ok(())
}

// eth_ subscription methods for flashblocks
#[tokio::test]
async fn test_eth_subscribe_new_flashblocks() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblocks"]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    assert_eq!(sub["jsonrpc"], "2.0");
    assert_eq!(sub["id"], 1);
    let subscription_id = sub["result"].as_str().expect("subscription id expected");

    setup.send_flashblock(setup.create_first_payload()).await?;

    let notification = ws_stream.next().await.unwrap()?;
    let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
    assert_eq!(notif["method"], "eth_subscription");
    assert_eq!(notif["params"]["subscription"], subscription_id);

    let block = &notif["params"]["result"];
    assert_eq!(block["number"], "0x1");
    assert!(block["hash"].is_string());
    assert!(block["parentHash"].is_string());
    assert!(block["transactions"].is_array());
    assert_eq!(block["transactions"].as_array().unwrap().len(), 1);

    Ok(())
}

#[tokio::test]
async fn test_eth_subscribe_multiple_flashblocks() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblocks"]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    let subscription_id = sub["result"].as_str().expect("subscription id expected");

    setup.send_flashblock(setup.create_first_payload()).await?;

    let notif1 = ws_stream.next().await.unwrap()?;
    let notif1: serde_json::Value = serde_json::from_str(notif1.to_text()?)?;
    assert_eq!(notif1["params"]["subscription"], subscription_id);

    let block1 = &notif1["params"]["result"];
    assert_eq!(block1["number"], "0x1");
    assert_eq!(block1["transactions"].as_array().unwrap().len(), 1);

    setup.send_flashblock(setup.create_second_payload()).await?;

    let notif2 = ws_stream.next().await.unwrap()?;
    let notif2: serde_json::Value = serde_json::from_str(notif2.to_text()?)?;
    assert_eq!(notif2["params"]["subscription"], subscription_id);

    let block2 = &notif2["params"]["result"];
    assert_eq!(block1["number"], block2["number"]); // Same block, incremental updates
    assert_eq!(block2["transactions"].as_array().unwrap().len(), 6);

    Ok(())
}

#[tokio::test]
async fn test_eth_unsubscribe() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblocks"]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    let subscription_id = sub["result"].as_str().expect("subscription id expected");

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "eth_unsubscribe",
                "params": [subscription_id]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let unsub = ws_stream.next().await.unwrap()?;
    let unsub: serde_json::Value = serde_json::from_str(unsub.to_text()?)?;
    assert_eq!(unsub["jsonrpc"], "2.0");
    assert_eq!(unsub["id"], 2);
    assert_eq!(unsub["result"], true);

    Ok(())
}

#[tokio::test]
async fn test_eth_subscribe_multiple_clients() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws1, _) = connect_async(&ws_url).await?;
    let (mut ws2, _) = connect_async(&ws_url).await?;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_subscribe",
        "params": ["newFlashblocks"]
    });
    ws1.send(Message::Text(req.to_string().into())).await?;
    ws2.send(Message::Text(req.to_string().into())).await?;

    let _sub1 = ws1.next().await.unwrap()?;
    let _sub2 = ws2.next().await.unwrap()?;

    setup.send_flashblock(setup.create_first_payload()).await?;

    let notif1 = ws1.next().await.unwrap()?;
    let notif1: serde_json::Value = serde_json::from_str(notif1.to_text()?)?;
    let notif2 = ws2.next().await.unwrap()?;
    let notif2: serde_json::Value = serde_json::from_str(notif2.to_text()?)?;

    assert_eq!(notif1["method"], "eth_subscription");
    assert_eq!(notif2["method"], "eth_subscription");

    let block1 = &notif1["params"]["result"];
    let block2 = &notif2["params"]["result"];
    assert_eq!(block1["number"], "0x1");
    assert_eq!(block1["number"], block2["number"]);
    assert_eq!(block1["hash"], block2["hash"]);

    Ok(())
}

/// Test that standard subscription types (newHeads) work correctly.
/// This verifies that our ExtendedSubscriptionKind properly proxies to reth's implementation.
#[tokio::test]
async fn test_eth_subscribe_new_heads() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // Subscribe to newHeads - this should be proxied to reth's standard implementation
    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newHeads"]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    assert_eq!(sub["jsonrpc"], "2.0");
    assert_eq!(sub["id"], 1);
    // Should return a subscription ID, confirming the subscription was accepted
    assert!(sub["result"].is_string(), "Expected subscription ID, got: {:?}", sub);

    Ok(())
}

#[tokio::test]
async fn test_eth_subscribe_new_flashblock_transactions_hashes() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // Subscribe to newFlashblockTransactions with default (hash only) mode
    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblockTransactions"]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    assert_eq!(sub["jsonrpc"], "2.0");
    assert_eq!(sub["id"], 1);
    let subscription_id = sub["result"].as_str().expect("subscription id expected");

    // Send first flashblock with L1 deposit tx
    setup.send_flashblock(setup.create_first_payload()).await?;

    let notification = ws_stream.next().await.unwrap()?;
    let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
    assert_eq!(notif["method"], "eth_subscription");
    assert_eq!(notif["params"]["subscription"], subscription_id);

    // Result should be an array of transaction hashes (strings)
    let txs = notif["params"]["result"].as_array().expect("expected array of tx hashes");
    assert_eq!(txs.len(), 1);
    assert!(txs[0].is_string(), "Expected hash string, got: {:?}", txs[0]);

    // Send second flashblock with more transactions
    setup.send_flashblock(setup.create_second_payload()).await?;

    let notification2 = ws_stream.next().await.unwrap()?;
    let notif2: serde_json::Value = serde_json::from_str(notification2.to_text()?)?;
    let txs2 = notif2["params"]["result"].as_array().expect("expected array of tx hashes");
    assert_eq!(txs2.len(), 6);
    assert!(txs2.iter().all(|tx| tx.is_string()));

    Ok(())
}

#[tokio::test]
async fn test_eth_subscribe_new_flashblock_transactions_full() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    // Subscribe to newFlashblockTransactions with full transaction objects (true)
    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblockTransactions", true]
            })
            .to_string()
            .into(),
        ))
        .await?;

    let response = ws_stream.next().await.unwrap()?;
    let sub: serde_json::Value = serde_json::from_str(response.to_text()?)?;
    assert_eq!(sub["jsonrpc"], "2.0");
    assert_eq!(sub["id"], 1);
    let subscription_id = sub["result"].as_str().expect("subscription id expected");

    // Send flashblocks
    setup.send_flashblock(setup.create_first_payload()).await?;

    let notification = ws_stream.next().await.unwrap()?;
    let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
    assert_eq!(notif["method"], "eth_subscription");
    assert_eq!(notif["params"]["subscription"], subscription_id);

    // Result should be an array of full transaction objects
    let txs = notif["params"]["result"].as_array().expect("expected array of transactions");
    assert_eq!(txs.len(), 1);
    // Full transaction objects have fields like "hash", "from", "to", etc.
    assert!(txs[0]["hash"].is_string(), "Expected full tx with hash field");
    assert!(txs[0]["blockNumber"].is_string(), "Expected full tx with blockNumber field");

    // Send second flashblock with more transactions
    setup.send_flashblock(setup.create_second_payload()).await?;

    let notification2 = ws_stream.next().await.unwrap()?;
    let notif2: serde_json::Value = serde_json::from_str(notification2.to_text()?)?;
    let txs2 = notif2["params"]["result"].as_array().expect("expected array of transactions");
    assert_eq!(txs2.len(), 6);
    assert!(txs2.iter().all(|tx| tx["hash"].is_string() && tx["blockNumber"].is_string()));

    Ok(())
}
