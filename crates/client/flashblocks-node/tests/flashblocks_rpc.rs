//! Integration tests covering the Flashblocks RPC surface area.

use std::str::FromStr;

use DoubleCounter::DoubleCounterInstance;
use alloy_consensus::Transaction;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256, address, b256, bytes};
use alloy_provider::Provider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types::simulate::{SimBlock, SimulatePayload};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::{TransactionInput, error::EthRpcErrorCode};
use base_client_node::test_utils::{Account, DoubleCounter, L1_BLOCK_INFO_DEPOSIT_TX};
use base_flashblocks_node::test_harness::FlashblocksHarness;
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use eyre::Result;
use futures_util::{SinkExt, StreamExt};
use op_alloy_network::{Optimism, ReceiptResponse, TransactionResponse};
use op_alloy_rpc_types::OpTransactionRequest;
use reth_revm::context::TransactionType;
use reth_rpc_eth_api::RpcReceipt;
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// LogEmitterB: Emits LOG1 with TEST_LOG_TOPIC_0 when called
// Runtime bytecode:
//   PUSH32 0x01       ; data to log (32 bytes of value 1)
//   PUSH1 0x00        ; memory offset to store data
//   MSTORE            ; store data at memory[0:32]
//   PUSH32 topic0     ; TEST_LOG_TOPIC_0
//   PUSH1 0x20        ; log data size (32 bytes)
//   PUSH1 0x00        ; log data offset
//   LOG1              ; emit log with 1 topic
//   STOP              ; end execution
const LOG_EMITTER_B_RUNTIME: &str = concat!(
    "7f",
    "0000000000000000000000000000000000000000000000000000000000000001", // PUSH32 data
    "6000",                                                             // PUSH1 0
    "52",                                                               // MSTORE
    "7f",
    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", // PUSH32 topic0
    "6020",                                                             // PUSH1 32
    "6000",                                                             // PUSH1 0
    "a1",                                                               // LOG1
    "00",                                                               // STOP
);

// LogEmitterA: Emits LOG2 with topic0 and topic1, then CALLs LogEmitterB
// Runtime bytecode (LOG_EMITTER_B_ADDR will be patched in):
//   PUSH32 data       ; 1 ETH in wei as log data
//   PUSH1 0x00        ; memory offset
//   MSTORE            ; store at memory[0:32]
//   PUSH32 topic1     ; TEST_LOG_TOPIC_1
//   PUSH32 topic0     ; TEST_LOG_TOPIC_0
//   PUSH1 0x20        ; size
//   PUSH1 0x00        ; offset
//   LOG2              ; emit log with 2 topics
//   ; Now CALL LogEmitterB
//   PUSH1 0x00        ; retSize
//   PUSH1 0x00        ; retOffset
//   PUSH1 0x00        ; argsSize
//   PUSH1 0x00        ; argsOffset
//   PUSH1 0x00        ; value
//   PUSH20 addr       ; LogEmitterB address (patched)
//   PUSH2 0xffff      ; gas
//   CALL              ; call LogEmitterB
//   STOP
fn log_emitter_a_runtime(log_emitter_b_addr: Address) -> String {
    // Convert address to hex without 0x prefix
    let addr_hex = format!("{log_emitter_b_addr:040x}");
    format!(
        concat!(
            "7f",
            "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // PUSH32 data (1 ETH)
            "6000",                                                             // PUSH1 0
            "52",                                                               // MSTORE
            "7f",
            "000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266", // PUSH32 topic1
            "7f",
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", // PUSH32 topic0
            "6020",                                                             // PUSH1 32
            "6000",                                                             // PUSH1 0
            "a2",                                                               // LOG2
            "6000",                                                             // PUSH1 0 (retSize)
            "6000", // PUSH1 0 (retOffset)
            "6000", // PUSH1 0 (argsSize)
            "6000", // PUSH1 0 (argsOffset)
            "6000", // PUSH1 0 (value)
            "73",
            "{addr}", // PUSH20 LogEmitterB address
            "61ffff", // PUSH2 0xffff (gas)
            "f1",     // CALL
            "00",     // STOP
        ),
        addr = addr_hex
    )
}

// Wrap runtime code in init code that returns it
// Init code: CODECOPY runtime to memory[0:size], RETURN it
fn wrap_in_init_code(runtime_hex: &str) -> Bytes {
    // Parse hex string (handle both with and without 0x prefix)
    let hex_str = runtime_hex.strip_prefix("0x").unwrap_or(runtime_hex);
    let mut runtime_bytes = Vec::new();
    for i in (0..hex_str.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex_str[i..i + 2], 16).expect("valid hex");
        runtime_bytes.push(byte);
    }
    let runtime_size = runtime_bytes.len();

    // Init code:
    //   PUSH1 runtime_size
    //   PUSH1 init_size (12 bytes)
    //   PUSH1 0
    //   CODECOPY
    //   PUSH1 runtime_size
    //   PUSH1 0
    //   RETURN
    // Total init: 12 bytes
    let init_size = 12u8;
    let mut init_code = vec![
        0x60,
        runtime_size as u8, // PUSH1 runtime_size
        0x60,
        init_size, // PUSH1 init_size
        0x60,
        0x00, // PUSH1 0
        0x39, // CODECOPY
        0x60,
        runtime_size as u8, // PUSH1 runtime_size
        0x60,
        0x00, // PUSH1 0
        0xf3, // RETURN
    ];
    init_code.extend(runtime_bytes);
    Bytes::from(init_code)
}

struct TestSetup {
    harness: FlashblocksHarness,
    txn_details: TransactionDetails,
}

struct TransactionDetails {
    counter_deployment_tx: Bytes,
    counter_address: Address,

    counter_increment_tx: Bytes,

    counter_increment2_tx: Bytes,

    alice_eth_transfer_tx: Bytes,
    alice_eth_transfer_hash: TxHash,

    // Log-emitting contracts for log tests
    log_emitter_b_deployment_tx: Bytes,
    log_emitter_b_address: Address,

    log_emitter_a_deployment_tx: Bytes,
    log_emitter_a_address: Address,

    log_trigger_tx: Bytes,
    log_trigger_hash: TxHash,

    // Balance transfer for balance test
    balance_transfer_tx: Bytes,
}

impl TestSetup {
    async fn new() -> Result<Self> {
        let harness = FlashblocksHarness::new().await?;

        let provider = harness.provider();
        let deployer = Account::Deployer;
        let alice = Account::Alice;
        let bob = Account::Bob;

        // DoubleCounter deployment at nonce 0
        let (counter_deployment_tx, counter_address, _) = deployer
            .create_deployment_tx(DoubleCounter::BYTECODE.clone(), 0)
            .expect("should be able to sign DoubleCounter deployment txn");
        let counter = DoubleCounterInstance::new(counter_address, provider);
        let (increment1_tx, _) = deployer
            .sign_txn_request(counter.increment().into_transaction_request().nonce(1))
            .expect("should be able to sign increment() txn");
        let (increment2_tx, _) = deployer
            .sign_txn_request(counter.increment2().into_transaction_request().nonce(2))
            .expect("should be able to sign increment2() txn");

        // Alice's ETH transfer at nonce 0
        let (eth_transfer_tx, eth_transfer_hash) = alice
            .sign_txn_request(
                OpTransactionRequest::default()
                    .from(alice.address())
                    .transaction_type(TransactionType::Eip1559.into())
                    .gas_limit(100_000)
                    .nonce(0)
                    .to(bob.address())
                    .value(U256::from_str("999999999000000000000000").unwrap()),
            )
            .expect("should be able to sign eth transfer txn");

        // Log-emitting contracts:
        // Deploy LogEmitterB at deployer nonce 3
        let log_emitter_b_address = deployer.address().create(3);
        let log_emitter_b_bytecode = wrap_in_init_code(LOG_EMITTER_B_RUNTIME);
        let (log_emitter_b_deployment_tx, _, _) = deployer
            .create_deployment_tx(log_emitter_b_bytecode, 3)
            .expect("should be able to sign LogEmitterB deployment txn");

        // Deploy LogEmitterA at deployer nonce 4 (knows LogEmitterB's address)
        let log_emitter_a_address = deployer.address().create(4);
        let log_emitter_a_runtime = log_emitter_a_runtime(log_emitter_b_address);
        let log_emitter_a_bytecode = wrap_in_init_code(&log_emitter_a_runtime);
        let (log_emitter_a_deployment_tx, _, _) = deployer
            .create_deployment_tx(log_emitter_a_bytecode, 4)
            .expect("should be able to sign LogEmitterA deployment txn");

        // Call LogEmitterA at deployer nonce 5 to trigger logs
        let (log_trigger_tx, log_trigger_hash) = deployer
            .sign_txn_request(
                OpTransactionRequest::default()
                    .from(deployer.address())
                    .transaction_type(TransactionType::Eip1559.into())
                    .gas_limit(100_000)
                    .nonce(5)
                    .to(log_emitter_a_address),
            )
            .expect("should be able to sign log trigger txn");

        // Balance transfer: alice sends PENDING_BALANCE wei to TEST_ADDRESS at nonce 1
        let (balance_transfer_tx, _) = alice
            .sign_txn_request(
                OpTransactionRequest::default()
                    .from(alice.address())
                    .transaction_type(TransactionType::Eip1559.into())
                    .gas_limit(21_000)
                    .nonce(1)
                    .to(TEST_ADDRESS)
                    .value(U256::from(PENDING_BALANCE)),
            )
            .expect("should be able to sign balance transfer txn");

        let txn_details = TransactionDetails {
            counter_deployment_tx,
            counter_address,
            counter_increment_tx: increment1_tx,
            counter_increment2_tx: increment2_tx,
            alice_eth_transfer_tx: eth_transfer_tx,
            alice_eth_transfer_hash: eth_transfer_hash,
            log_emitter_b_deployment_tx,
            log_emitter_b_address,
            log_emitter_a_deployment_tx,
            log_emitter_a_address,
            log_trigger_tx,
            log_trigger_hash,
            balance_transfer_tx,
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
            metadata: Metadata { block_number: 1 },
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
                    // Log-emitting contracts and trigger
                    self.txn_details.log_emitter_b_deployment_tx.clone(),
                    self.txn_details.log_emitter_a_deployment_tx.clone(),
                    self.txn_details.log_trigger_tx.clone(),
                    // Balance transfer to TEST_ADDRESS
                    self.txn_details.balance_transfer_tx.clone(),
                ],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: Metadata { block_number: 1 },
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
const DEPOSIT_GAS_USED: u64 = 24770;
const DEPOSIT_TX_HASH: TxHash =
    b256!("0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548");

// Test log topics - these represent common events
const TEST_LOG_TOPIC_0: B256 =
    b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"); // Transfer event
const TEST_LOG_TOPIC_1: B256 =
    b256!("0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"); // From address

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

    // Query pending block after sending the second payload with transactions
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Pending)
        .await?
        .expect("pending block expected");

    assert_eq!(block.number(), 1);
    // First flashblock: 1 L1Info transaction
    // Second flashblock: 1 DEPOSIT_TX + 1 alice ETH transfer + 1 counter deploy + 1 counter increment + 1 counter increment2
    // + 1 LogEmitterB deploy + 1 LogEmitterA deploy + 1 log trigger + 1 balance transfer
    // Total: 1 + 9 = 10 transactions
    assert_eq!(block.transactions.hashes().len(), 10);

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
    assert_eq!(tx2.from(), Account::Alice.address());
    assert_eq!(tx2.inner.inner.as_eip1559().unwrap().to().unwrap(), Account::Bob.address());

    Ok(())
}

#[tokio::test]
async fn test_get_transaction_receipt_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let receipt = provider.get_transaction_receipt(DEPOSIT_TX_HASH).await?;
    assert!(receipt.is_none());

    setup.send_test_payloads().await?;

    let receipt =
        provider.get_transaction_receipt(DEPOSIT_TX_HASH).await?.expect("receipt expected");
    assert_eq!(receipt.gas_used(), DEPOSIT_GAS_USED);

    let receipt = provider
        .get_transaction_receipt(setup.txn_details.alice_eth_transfer_hash)
        .await?
        .expect("receipt expected");
    assert_eq!(receipt.gas_used(), 21000);

    Ok(())
}

#[tokio::test]
async fn test_get_transaction_count() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    let deployer_addr = Account::Deployer.address();
    let alice_addr = Account::Alice.address();

    assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).pending().await?, 0);
    assert_eq!(provider.get_transaction_count(deployer_addr).pending().await?, 0);
    assert_eq!(provider.get_transaction_count(alice_addr).pending().await?, 0);

    setup.send_test_payloads().await?;

    assert_eq!(provider.get_transaction_count(DEPOSIT_SENDER).pending().await?, 2);
    // Deployer has: counter deploy (0), counter increment (1), counter increment2 (2),
    // LogEmitterB deploy (3), LogEmitterA deploy (4), log trigger (5) = nonce 6
    assert_eq!(provider.get_transaction_count(deployer_addr).pending().await?, 6);
    // Alice has: big ETH transfer (0), balance transfer to TEST_ADDRESS (1) = nonce 2
    assert_eq!(provider.get_transaction_count(alice_addr).pending().await?, 2);

    Ok(())
}

#[tokio::test]
async fn test_eth_call() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    // Initially, the big spend will succeed because we haven't sent the test payloads yet
    let big_spend = OpTransactionRequest::default()
        .from(Account::Alice.address())
        .transaction_type(0)
        .gas_limit(200000)
        .nonce(0)
        .to(Account::Bob.address())
        .value(U256::from(9999999999849942300000u128));

    let res = provider.call(big_spend.clone()).block(BlockNumberOrTag::Pending.into()).await;
    assert!(res.is_ok());

    setup.send_test_payloads().await?;

    // We included a big spending transaction in the payloads
    // and now don't have enough funds for this request, so this eth_call will fail
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
        .from(Account::Alice.address())
        .transaction_type(0)
        .gas_limit(200000)
        .nonce(0)
        .to(Account::Bob.address())
        .value(U256::from(9999999999849942300000u128))
        .input(TransactionInput::new(bytes!("0x")));

    let res = provider
        .estimate_gas(send_estimate_gas.clone())
        .block(BlockNumberOrTag::Pending.into())
        .await;

    assert!(res.is_ok());

    setup.send_test_payloads().await?;

    // We included a heavy spending transaction and now don't have enough funds for this request, so
    // this eth_estimate_gas will fail
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
    assert!(receipt_result.err().unwrap().to_string().contains(format!("{error_code}").as_str()));
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

    // We should now have 2 logs from the log_trigger_tx transaction
    assert_eq!(logs.len(), 2);

    // Verify the first log is from LogEmitterA
    assert_eq!(logs[0].address(), setup.txn_details.log_emitter_a_address);
    assert_eq!(logs[0].topics()[0], TEST_LOG_TOPIC_0);
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.log_trigger_hash));

    // Verify the second log is from LogEmitterB
    assert_eq!(logs[1].address(), setup.txn_details.log_emitter_b_address);
    assert_eq!(logs[1].topics()[0], TEST_LOG_TOPIC_0);
    assert_eq!(logs[1].transaction_hash, Some(setup.txn_details.log_trigger_hash));

    Ok(())
}

#[tokio::test]
async fn test_get_logs_filter_by_address() -> Result<()> {
    let setup = TestSetup::new().await?;
    let provider = setup.harness.provider();

    setup.send_test_payloads().await?;

    // Test filtering by LogEmitterA address
    let logs = provider
        .get_logs(
            &alloy_rpc_types_eth::Filter::default()
                .address(setup.txn_details.log_emitter_a_address)
                .from_block(alloy_eips::BlockNumberOrTag::Pending)
                .to_block(alloy_eips::BlockNumberOrTag::Pending),
        )
        .await?;

    // Should get only 1 log from LogEmitterA
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].address(), setup.txn_details.log_emitter_a_address);
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.log_trigger_hash));

    // Test filtering by LogEmitterB address
    let logs = provider
        .get_logs(
            &alloy_rpc_types_eth::Filter::default()
                .address(setup.txn_details.log_emitter_b_address)
                .from_block(alloy_eips::BlockNumberOrTag::Pending)
                .to_block(alloy_eips::BlockNumberOrTag::Pending),
        )
        .await?;

    // Should get only 1 log from LogEmitterB
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].address(), setup.txn_details.log_emitter_b_address);
    assert_eq!(logs[0].transaction_hash, Some(setup.txn_details.log_trigger_hash));

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

    // Test filtering by specific topic combination - should match only LogEmitterA (has 2 topics)
    let filter = alloy_rpc_types_eth::Filter::default()
        .topic1(TEST_LOG_TOPIC_1)
        .from_block(alloy_eips::BlockNumberOrTag::Pending)
        .to_block(alloy_eips::BlockNumberOrTag::Pending);

    let logs = provider.get_logs(&filter).await?;

    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].address(), setup.txn_details.log_emitter_a_address);
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
        logs.iter().all(|log| log.transaction_hash == Some(setup.txn_details.log_trigger_hash))
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
        logs.iter().all(|log| log.transaction_hash == Some(setup.txn_details.log_trigger_hash))
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
        logs.iter().all(|log| log.transaction_hash == Some(setup.txn_details.log_trigger_hash))
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
    assert_eq!(block2["transactions"].as_array().unwrap().len(), 10); // 1 from first + 9 from second

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
/// This verifies that our `ExtendedSubscriptionKind` properly proxies to reth's implementation.
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
    assert!(sub["result"].is_string(), "Expected subscription ID, got: {sub:?}");

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

    // Each transaction is now sent as a separate message (one tx per message)
    let notification = ws_stream.next().await.unwrap()?;
    let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
    assert_eq!(notif["method"], "eth_subscription");
    assert_eq!(notif["params"]["subscription"], subscription_id);

    // Result should be a single transaction hash (string), not an array
    let tx_hash = &notif["params"]["result"];
    assert!(tx_hash.is_string(), "Expected hash string, got: {tx_hash:?}");

    // Send second flashblock with 9 more transactions (delta only, not cumulative)
    setup.send_flashblock(setup.create_second_payload()).await?;

    // Receive 9 separate messages (one per transaction in the delta)
    let mut received_hashes = Vec::new();
    for _ in 0..9 {
        let notification = ws_stream.next().await.unwrap()?;
        let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
        assert_eq!(notif["params"]["subscription"], subscription_id);
        let tx_hash = notif["params"]["result"].as_str().expect("expected hash string");
        received_hashes.push(tx_hash.to_string());
    }
    assert_eq!(received_hashes.len(), 9);

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

    // Each transaction is now sent as a separate message (one tx per message)
    let notification = ws_stream.next().await.unwrap()?;
    let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
    assert_eq!(notif["method"], "eth_subscription");
    assert_eq!(notif["params"]["subscription"], subscription_id);

    // Result should be a single full transaction object with logs, not an array
    let tx = &notif["params"]["result"];
    assert!(tx.is_object(), "Expected transaction object, got: {tx:?}");
    assert!(tx["hash"].is_string(), "Expected full tx with hash field");
    assert!(tx["blockNumber"].is_string(), "Expected full tx with blockNumber field");
    assert!(tx["logs"].is_array(), "Expected logs array in full transaction");

    // Send second flashblock with 9 more transactions (delta only, not cumulative)
    setup.send_flashblock(setup.create_second_payload()).await?;

    // Receive 9 separate messages (one per transaction in the delta)
    let mut received_count = 0;
    for _ in 0..9 {
        let notification = ws_stream.next().await.unwrap()?;
        let notif: serde_json::Value = serde_json::from_str(notification.to_text()?)?;
        assert_eq!(notif["params"]["subscription"], subscription_id);
        let tx = &notif["params"]["result"];
        assert!(tx["hash"].is_string() && tx["blockNumber"].is_string());
        assert!(tx["logs"].is_array(), "Expected logs array in full transaction");
        received_count += 1;
    }
    assert_eq!(received_count, 9);

    Ok(())
}

#[tokio::test]
async fn test_get_block_transaction_count_by_number_pending() -> Result<()> {
    let setup = TestSetup::new().await?;
    let url = setup.harness.rpc_url();
    let client = RpcClient::new_http(url.parse()?);

    // Query pending block transaction count when no flashblocks exist
    // Should fall back to latest block (block 0 with 0 transactions)
    let count: Option<U256> =
        client.request("eth_getBlockTransactionCountByNumber", ("pending",)).await?;
    assert_eq!(count, Some(U256::from(0)));

    // Send first flashblock with 1 transaction (L1Info deposit)
    setup.send_flashblock(setup.create_first_payload()).await?;

    let count: Option<U256> =
        client.request("eth_getBlockTransactionCountByNumber", ("pending",)).await?;
    assert_eq!(count, Some(U256::from(1)));

    // Send second flashblock with 9 more transactions
    setup.send_flashblock(setup.create_second_payload()).await?;

    let count: Option<U256> =
        client.request("eth_getBlockTransactionCountByNumber", ("pending",)).await?;
    // Total: 1 (L1Info) + 9 (second payload) = 10 transactions
    assert_eq!(count, Some(U256::from(10)));

    // Query non-pending block (latest = block 0)
    let count: Option<U256> =
        client.request("eth_getBlockTransactionCountByNumber", ("latest",)).await?;
    assert_eq!(count, Some(U256::from(0)));

    Ok(())
}
