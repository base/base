//! Integration tests covering the Flashblocks RPC surface area.

use std::str::FromStr;

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
use alloy_sol_macro::sol;
use base_reth_flashblocks::{Flashblock, Metadata};
use base_reth_test_utils::{
    fixtures::{BLOCK_INFO_TXN, BLOCK_INFO_TXN_HASH},
    flashblocks_harness::FlashblocksHarness,
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

use crate::DoubleCounterContract::DoubleCounterContractInstance;

/// Encodes TransparentProxy constructor arguments: (address implementation, address admin)
fn encode_proxy_constructor(implementation: Address, admin: Address) -> Vec<u8> {
    use alloy_sol_types::SolType;
    type ConstructorArgs = (
        alloy_sol_types::sol_data::Address,
        alloy_sol_types::sol_data::Address,
    );
    ConstructorArgs::abi_encode_params(&(implementation, admin))
}

sol!(
    #[sol(rpc)]
    DoubleCounterContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/DoubleCounter.sol/DoubleCounter.json"
    )
);

sol!(
    #[sol(rpc)]
    TestTokenContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/TestToken.sol/TestToken.json"
    )
);

sol!(
    #[sol(rpc)]
    SimpleTokenContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/SimpleToken.sol/SimpleToken.json"
    )
);

sol!(
    #[sol(rpc)]
    TransparentProxyContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/TransparentProxy.sol/TransparentProxy.json"
    )
);

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
            .create_deployment_tx(DoubleCounterContract::BYTECODE.clone(), 0)
            .expect("should be able to sign DoubleCounter deployment txn");
        let counter = DoubleCounterContractInstance::new(counter_address.clone(), provider);
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
        let counter = DoubleCounterContractInstance::new(
            self.txn_details.counter_address,
            self.harness.provider(),
        );
        counter.count1().into_transaction_request()
    }

    fn count2(&self) -> OpTransactionRequest {
        let counter = DoubleCounterContractInstance::new(
            self.txn_details.counter_address,
            self.harness.provider(),
        );
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

// base_ methods
#[tokio::test]
async fn test_base_subscribe_new_flashblocks() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "base_subscribe",
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
    assert_eq!(notif["method"], "base_subscription");
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
async fn test_base_subscribe_multiple_flashblocks() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "base_subscribe",
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
async fn test_base_unsubscribe() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws_stream, _) = connect_async(&ws_url).await?;

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "base_subscribe",
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
                "method": "base_unsubscribe",
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
async fn test_base_subscribe_multiple_clients() -> eyre::Result<()> {
    let setup = TestSetup::new().await?;
    let _provider = setup.harness.provider();
    let ws_url = setup.harness.ws_url();
    let (mut ws1, _) = connect_async(&ws_url).await?;
    let (mut ws2, _) = connect_async(&ws_url).await?;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "base_subscribe",
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

    assert_eq!(notif1["method"], "base_subscription");
    assert_eq!(notif2["method"], "base_subscription");

    let block1 = &notif1["params"]["result"];
    let block2 = &notif2["params"]["result"];
    assert_eq!(block1["number"], "0x1");
    assert_eq!(block1["number"], block2["number"]);
    assert_eq!(block1["hash"], block2["hash"]);

    Ok(())
}

// ============================================================================
// ERC-20 eth_call tests
// ============================================================================

/// ERC-20 Test Setup following the same pattern as TestSetup.
/// Uses the same flashblock structure that's proven to work.
struct ERC20TestSetup {
    harness: FlashblocksHarness,
    token_address: Address,
    mint_amount: U256,
}

impl ERC20TestSetup {
    async fn new() -> Result<Self> {
        let harness = FlashblocksHarness::new().await?;
        let provider = harness.provider();
        let deployer = &harness.accounts().deployer;
        let alice = &harness.accounts().alice;

        // Deploy SimpleToken (no constructor args, like DoubleCounter)
        let (token_deploy_tx, token_address, token_deploy_hash) = deployer
            .create_deployment_tx(SimpleTokenContract::BYTECODE.clone(), 0)
            .expect("should sign token deployment");

        use crate::SimpleTokenContract::SimpleTokenContractInstance;
        let token = SimpleTokenContractInstance::new(token_address, provider.clone());

        // Mint tokens to Alice
        let mint_amount = U256::from(1000u64);
        let (mint_tx, mint_tx_hash) = deployer
            .sign_txn_request(
                token
                    .mint(alice.address, mint_amount)
                    .into_transaction_request()
                    .nonce(1),
            )
            .expect("should sign mint txn");

        // First flashblock: base with L1 info
        let first_flashblock = Flashblock {
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
        };

        harness.send_flashblock(first_flashblock).await?;

        // Second flashblock: includes DEPOSIT_TX (like the working tests), deploy, and mint
        let second_flashblock = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                blob_gas_used: Some(0),
                transactions: vec![DEPOSIT_TX, token_deploy_tx, mint_tx],
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
                        token_deploy_hash,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 800000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        mint_tx_hash,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 850000,
                            logs: vec![alloy_primitives::Log {
                                address: token_address,
                                data: LogData::new(
                                    vec![
                                        // Transfer event signature
                                        b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                                        B256::ZERO,
                                        B256::left_padding_from(alice.address.as_slice()),
                                    ],
                                    alloy_primitives::Bytes::from(mint_amount.to_be_bytes_vec()),
                                )
                                .unwrap(),
                            }],
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
            },
        };

        harness.send_flashblock(second_flashblock).await?;

        Ok(Self {
            harness,
            token_address,
            mint_amount,
        })
    }
}

/// Tests basic ERC-20 functionality via eth_call including:
/// - balanceOf: Check token balance
/// - totalSupply: Check total supply
/// - State changes via mint reflected in eth_call
#[tokio::test]
async fn test_eth_call_erc20_balance_of() -> Result<()> {
    let setup = ERC20TestSetup::new().await?;
    let provider = setup.harness.provider();
    let alice = &setup.harness.accounts().alice;

    use crate::SimpleTokenContract::SimpleTokenContractInstance;
    let token = SimpleTokenContractInstance::new(setup.token_address, provider.clone());

    // Test balanceOf via eth_call - should return minted amount
    let balance_call = token.balanceOf(alice.address).into_transaction_request();
    let balance_result = provider.call(balance_call).await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(balance, setup.mint_amount, "Alice's balance should equal minted amount");

    // Test balanceOf for address with no tokens
    let zero_balance_call = token
        .balanceOf(setup.harness.accounts().bob.address)
        .into_transaction_request();
    let zero_balance_result = provider.call(zero_balance_call).await?;
    let zero_balance = U256::from_str(&zero_balance_result.to_string())?;
    assert_eq!(zero_balance, U256::ZERO, "Bob's balance should be zero");

    // Test totalSupply via eth_call
    let supply_call = token.totalSupply().into_transaction_request();
    let supply_result = provider.call(supply_call).await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(supply, setup.mint_amount, "Total supply should equal minted amount");

    Ok(())
}

/// Tests ERC-20 transfer functionality via eth_call
#[tokio::test]
async fn test_eth_call_erc20_transfer() -> Result<()> {
    let harness = FlashblocksHarness::new().await?;
    let provider = harness.provider();
    let deployer = &harness.accounts().deployer;
    let alice = &harness.accounts().alice;
    let bob = &harness.accounts().bob;

    // Deploy SimpleToken (no constructor args)
    let (token_deploy_tx, token_address, token_deploy_hash) = deployer
        .create_deployment_tx(SimpleTokenContract::BYTECODE.clone(), 0)
        .expect("should sign token deployment");

    use crate::SimpleTokenContract::SimpleTokenContractInstance;
    let token = SimpleTokenContractInstance::new(token_address, provider.clone());

    // Mint tokens to Alice
    let mint_amount = U256::from(1000u64);
    let (mint_tx, mint_tx_hash) = deployer
        .sign_txn_request(
            token
                .mint(alice.address, mint_amount)
                .into_transaction_request()
                .nonce(1),
        )
        .expect("should sign mint txn");

    // Alice transfers tokens to Bob
    let transfer_amount = U256::from(100u64);
    let (transfer_tx, transfer_tx_hash) = alice
        .sign_txn_request(
            token
                .transfer(bob.address, transfer_amount)
                .into_transaction_request()
                .nonce(0),
        )
        .expect("should sign transfer txn");

    // First flashblock: base with L1 info
    let first_flashblock = Flashblock {
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
    };

    harness.send_flashblock(first_flashblock).await?;

    // Second flashblock: includes DEPOSIT_TX, deploy, mint, and transfer
    let second_flashblock = Flashblock {
        payload_id: PayloadId::new([0; 8]),
        index: 1,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: 0,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![DEPOSIT_TX, token_deploy_tx, mint_tx, transfer_tx],
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
                    token_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 800000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    mint_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 850000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    transfer_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 900000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        },
    };

    harness.send_flashblock(second_flashblock).await?;

    // Verify Alice's balance decreased
    let alice_balance_call = token.balanceOf(alice.address).into_transaction_request();
    let alice_balance_result = provider
        .call(alice_balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let alice_balance = U256::from_str(&alice_balance_result.to_string())?;
    assert_eq!(
        alice_balance,
        mint_amount - transfer_amount,
        "Alice's balance should be reduced by transfer amount"
    );

    // Verify Bob received the tokens
    let bob_balance_call = token.balanceOf(bob.address).into_transaction_request();
    let bob_balance_result = provider
        .call(bob_balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(bob_balance, transfer_amount, "Bob should have received transfer amount");

    Ok(())
}

/// Tests ERC-20 allowance and transferFrom via eth_call
#[tokio::test]
async fn test_eth_call_erc20_allowance() -> Result<()> {
    let harness = FlashblocksHarness::new().await?;
    let provider = harness.provider();
    let deployer = &harness.accounts().deployer;
    let alice = &harness.accounts().alice;
    let bob = &harness.accounts().bob;

    // Deploy SimpleToken (no constructor args)
    let (token_deploy_tx, token_address, token_deploy_hash) = deployer
        .create_deployment_tx(SimpleTokenContract::BYTECODE.clone(), 0)
        .expect("should sign token deployment");

    use crate::SimpleTokenContract::SimpleTokenContractInstance;
    let token = SimpleTokenContractInstance::new(token_address, provider.clone());

    // Mint tokens to Alice
    let mint_amount = U256::from(1000u64);
    let (mint_tx, mint_tx_hash) = deployer
        .sign_txn_request(
            token
                .mint(alice.address, mint_amount)
                .into_transaction_request()
                .nonce(1),
        )
        .expect("should sign mint txn");

    // Alice approves Bob to spend tokens
    let approval_amount = U256::from(500u64);
    let (approve_tx, approve_tx_hash) = alice
        .sign_txn_request(
            token
                .approve(bob.address, approval_amount)
                .into_transaction_request()
                .nonce(0),
        )
        .expect("should sign approve txn");

    // First flashblock: base with L1 info
    let first_flashblock = Flashblock {
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
    };

    harness.send_flashblock(first_flashblock).await?;

    // Second flashblock: includes DEPOSIT_TX, deploy, mint, and approve
    let second_flashblock = Flashblock {
        payload_id: PayloadId::new([0; 8]),
        index: 1,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: 0,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![DEPOSIT_TX, token_deploy_tx, mint_tx, approve_tx],
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
                    token_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 800000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    mint_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 850000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    approve_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 900000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        },
    };

    harness.send_flashblock(second_flashblock).await?;

    // Check allowance via eth_call
    let allowance_call = token.allowance(alice.address, bob.address).into_transaction_request();
    let allowance_result = provider
        .call(allowance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let allowance = U256::from_str(&allowance_result.to_string())?;
    assert_eq!(allowance, approval_amount, "Allowance should equal approval amount");

    // Check zero allowance for non-approved spender
    let zero_allowance_call = token
        .allowance(alice.address, harness.accounts().charlie.address)
        .into_transaction_request();
    let zero_allowance_result = provider
        .call(zero_allowance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let zero_allowance = U256::from_str(&zero_allowance_result.to_string())?;
    assert_eq!(zero_allowance, U256::ZERO, "Non-approved spender should have zero allowance");

    Ok(())
}

// ============================================================================
// TransparentProxy eth_call tests
// ============================================================================

/// Tests eth_call through a TransparentProxy contract.
/// This tests the delegatecall pattern used by USDC and other major tokens.
/// The proxy delegates calls to the implementation (SimpleToken), but state is stored in proxy.
#[tokio::test]
async fn test_eth_call_transparent_proxy_erc20() -> Result<()> {
    let harness = FlashblocksHarness::new().await?;
    let provider = harness.provider();
    let deployer = &harness.accounts().deployer;
    let alice = &harness.accounts().alice;
    // Charlie will be the proxy admin (can't call through proxy)
    let proxy_admin = &harness.accounts().charlie;

    // Step 1: Deploy the implementation contract (SimpleToken - no constructor args)
    let (impl_deploy_tx, impl_address, impl_deploy_hash) = deployer
        .create_deployment_tx(SimpleTokenContract::BYTECODE.clone(), 0)
        .expect("should sign implementation deployment");

    // Step 2: Deploy the TransparentProxy pointing to the implementation
    let proxy_deploy_data = [
        TransparentProxyContract::BYTECODE.to_vec(),
        encode_proxy_constructor(impl_address, proxy_admin.address),
    ]
    .concat();

    let (proxy_deploy_tx, proxy_address, proxy_deploy_hash) = deployer
        .create_deployment_tx(proxy_deploy_data.into(), 1)
        .expect("should sign proxy deployment");

    // Create a token instance pointing to the PROXY address (not implementation)
    use crate::SimpleTokenContract::SimpleTokenContractInstance;
    let proxied_token = SimpleTokenContractInstance::new(proxy_address, provider.clone());

    // Step 3: Mint tokens through the proxy
    let mint_amount = U256::from(1_000_000u64);
    let (mint_tx, mint_tx_hash) = deployer
        .sign_txn_request(
            proxied_token
                .mint(alice.address, mint_amount)
                .into_transaction_request()
                .nonce(2),
        )
        .expect("should sign mint txn through proxy");

    // First flashblock: base with L1 info
    let first_flashblock = Flashblock {
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
    };

    harness.send_flashblock(first_flashblock).await?;

    // Second flashblock: includes DEPOSIT_TX, impl deploy, proxy deploy, and mint
    let second_flashblock = Flashblock {
        payload_id: PayloadId::new([0; 8]),
        index: 1,
        base: None,
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::default(),
            receipts_root: B256::default(),
            gas_used: 0,
            block_hash: B256::default(),
            blob_gas_used: Some(0),
            transactions: vec![DEPOSIT_TX, impl_deploy_tx, proxy_deploy_tx, mint_tx],
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
                    impl_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 800000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    proxy_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 1200000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    mint_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 1300000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        },
    };

    harness.send_flashblock(second_flashblock).await?;

    // Test: eth_call balanceOf through proxy should return minted amount
    // This tests that delegatecall correctly reads state from proxy storage
    let balance_call = proxied_token.balanceOf(alice.address).into_transaction_request();
    let balance_result = provider
        .call(balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let balance = U256::from_str(&balance_result.to_string())?;
    assert_eq!(
        balance, mint_amount,
        "Balance through proxy should equal minted amount"
    );

    // Test: eth_call totalSupply through proxy
    let supply_call = proxied_token.totalSupply().into_transaction_request();
    let supply_result = provider
        .call(supply_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let supply = U256::from_str(&supply_result.to_string())?;
    assert_eq!(
        supply, mint_amount,
        "Total supply through proxy should equal minted amount"
    );

    // Test: Implementation contract should have ZERO balance
    // (state is stored in proxy, not implementation)
    let impl_token = SimpleTokenContractInstance::new(impl_address, provider.clone());
    let impl_balance_call = impl_token.balanceOf(alice.address).into_transaction_request();
    let impl_balance_result = provider
        .call(impl_balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let impl_balance = U256::from_str(&impl_balance_result.to_string())?;
    assert_eq!(
        impl_balance,
        U256::ZERO,
        "Implementation should have zero balance (state in proxy)"
    );

    Ok(())
}

/// Tests transfer functionality through a TransparentProxy.
/// Verifies that delegatecall-based transfers work correctly with eth_call.
#[tokio::test]
async fn test_eth_call_transparent_proxy_transfer() -> Result<()> {
    let harness = FlashblocksHarness::new().await?;
    let provider = harness.provider();
    let deployer = &harness.accounts().deployer;
    let alice = &harness.accounts().alice;
    let bob = &harness.accounts().bob;
    let proxy_admin = &harness.accounts().charlie;

    // Deploy implementation (SimpleToken)
    let (impl_deploy_tx, impl_address, impl_deploy_hash) = deployer
        .create_deployment_tx(SimpleTokenContract::BYTECODE.clone(), 0)
        .expect("should deploy implementation");

    // Deploy proxy
    let proxy_deploy_data = [
        TransparentProxyContract::BYTECODE.to_vec(),
        encode_proxy_constructor(impl_address, proxy_admin.address),
    ]
    .concat();

    let (proxy_deploy_tx, proxy_address, proxy_deploy_hash) = deployer
        .create_deployment_tx(proxy_deploy_data.into(), 1)
        .expect("should deploy proxy");

    use crate::SimpleTokenContract::SimpleTokenContractInstance;
    let proxied_token = SimpleTokenContractInstance::new(proxy_address, provider.clone());

    // Mint to Alice
    let mint_amount = U256::from(10_000_000u64);
    let (mint_tx, mint_tx_hash) = deployer
        .sign_txn_request(
            proxied_token
                .mint(alice.address, mint_amount)
                .into_transaction_request()
                .nonce(2),
        )
        .expect("should sign mint");

    // Alice transfers to Bob through proxy
    let transfer_amount = U256::from(1_000_000u64);
    let (transfer_tx, transfer_tx_hash) = alice
        .sign_txn_request(
            proxied_token
                .transfer(bob.address, transfer_amount)
                .into_transaction_request()
                .nonce(0),
        )
        .expect("should sign transfer");

    // First flashblock: base with L1 info
    let first_flashblock = Flashblock {
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
    };

    harness.send_flashblock(first_flashblock).await?;

    // Second flashblock: includes all transactions
    let second_flashblock = Flashblock {
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
                impl_deploy_tx,
                proxy_deploy_tx,
                mint_tx,
                transfer_tx,
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
                    impl_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 800000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    proxy_deploy_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 1200000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    mint_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 1300000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    transfer_tx_hash,
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 1400000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: HashMap::default(),
        },
    };

    harness.send_flashblock(second_flashblock).await?;

    // Verify balances through proxy via eth_call
    let alice_balance_call = proxied_token.balanceOf(alice.address).into_transaction_request();
    let alice_balance_result = provider
        .call(alice_balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let alice_balance = U256::from_str(&alice_balance_result.to_string())?;
    assert_eq!(
        alice_balance,
        mint_amount - transfer_amount,
        "Alice's balance through proxy should be reduced"
    );

    let bob_balance_call = proxied_token.balanceOf(bob.address).into_transaction_request();
    let bob_balance_result = provider
        .call(bob_balance_call)
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let bob_balance = U256::from_str(&bob_balance_result.to_string())?;
    assert_eq!(
        bob_balance, transfer_amount,
        "Bob should have received tokens through proxy"
    );

    Ok(())
}
