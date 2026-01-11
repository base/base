//! Integration tests that stress Flashblocks state handling.

use std::{sync::Arc, time::Duration};

use alloy_consensus::{Receipt, Transaction};
use alloy_eips::{BlockHashOrNumber, Encodable2718};
use alloy_primitives::{
    Address, B256, BlockNumber, Bytes, U256, hex::FromHex, map::foldhash::HashMap,
};
use alloy_rpc_types_engine::PayloadId;
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use base_reth_flashblocks::{FlashblocksAPI, FlashblocksState, PendingBlocksAPI};
use base_reth_test_utils::{
    FlashblocksHarness, L1_BLOCK_INFO_DEPOSIT_TX, L1_BLOCK_INFO_DEPOSIT_TX_HASH, LocalNodeProvider,
    TestAccounts,
};
use op_alloy_consensus::OpDepositReceipt;
use op_alloy_network::BlockResponse;
use reth::{
    chainspec::EthChainSpec,
    providers::{AccountReader, BlockNumReader, BlockReader},
    transaction_pool::test_utils::TransactionBuilder,
};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_primitives_traits::{Account, Block as BlockT, RecoveredBlock};
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use tokio::time::sleep;
// The amount of time to wait (in milliseconds) after sending a new flashblock or canonical block
// so it can be processed by the state processor
const SLEEP_TIME: u64 = 10;

#[derive(Eq, PartialEq, Debug, Hash, Clone, Copy)]
enum User {
    Alice,
    Bob,
    Charlie,
}

struct TestHarness {
    node: FlashblocksHarness,
    flashblocks: Arc<FlashblocksState<LocalNodeProvider>>,
    provider: LocalNodeProvider,
    user_to_address: HashMap<User, Address>,
    user_to_private_key: HashMap<User, B256>,
}

impl TestHarness {
    async fn new() -> Self {
        // These tests simulate pathological timing (missing receipts, reorgs, etc.), so we disable
        // the automatic canonical listener and only apply blocks when the test explicitly requests it.
        let node = FlashblocksHarness::manual_canonical()
            .await
            .expect("able to launch flashblocks harness");
        let provider = node.blockchain_provider();
        let flashblocks = node.flashblocks_state();

        let genesis_block = provider
            .block(BlockHashOrNumber::Number(0))
            .expect("able to load block")
            .expect("block exists")
            .try_into_recovered()
            .expect("able to recover block");
        flashblocks.on_canonical_block_received(genesis_block);

        let accounts: TestAccounts = node.accounts().clone();

        let mut user_to_address = HashMap::default();
        user_to_address.insert(User::Alice, accounts.alice.address);
        user_to_address.insert(User::Bob, accounts.bob.address);
        user_to_address.insert(User::Charlie, accounts.charlie.address);

        let mut user_to_private_key = HashMap::default();
        user_to_private_key
            .insert(User::Alice, Self::decode_private_key(accounts.alice.private_key));
        user_to_private_key.insert(User::Bob, Self::decode_private_key(accounts.bob.private_key));
        user_to_private_key
            .insert(User::Charlie, Self::decode_private_key(accounts.charlie.private_key));

        Self { node, flashblocks, provider, user_to_address, user_to_private_key }
    }

    fn decode_private_key(key: &str) -> B256 {
        B256::from_hex(key).expect("valid hex-encoded key")
    }

    fn address(&self, u: User) -> Address {
        assert!(self.user_to_address.contains_key(&u));
        self.user_to_address[&u]
    }

    fn signer(&self, u: User) -> B256 {
        assert!(self.user_to_private_key.contains_key(&u));
        self.user_to_private_key[&u]
    }

    fn canonical_account(&self, u: User) -> Account {
        self.provider
            .basic_account(&self.address(u))
            .expect("can lookup account state")
            .expect("should be existing account state")
    }

    fn canonical_balance(&self, u: User) -> U256 {
        self.canonical_account(u).balance
    }

    fn expected_pending_balance(&self, u: User, delta: u128) -> U256 {
        self.canonical_balance(u) + U256::from(delta)
    }

    fn account_state(&self, u: User) -> Account {
        let basic_account = self.canonical_account(u);

        let nonce = self
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(self.address(u))
            .to::<u64>();
        let balance = self
            .flashblocks
            .get_pending_blocks()
            .get_balance(self.address(u))
            .unwrap_or(basic_account.balance);

        Account {
            nonce: nonce + basic_account.nonce,
            balance,
            bytecode_hash: basic_account.bytecode_hash,
        }
    }

    fn build_transaction_to_send_eth(
        &self,
        from: User,
        to: User,
        amount: u128,
    ) -> OpTransactionSigned {
        let txn = TransactionBuilder::default()
            .signer(self.signer(from))
            .chain_id(self.provider.chain_spec().chain_id())
            .to(self.address(to))
            .nonce(self.account_state(from).nonce)
            .value(amount)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559()
            .as_eip1559()
            .unwrap()
            .clone();

        OpTransactionSigned::Eip1559(txn)
    }

    fn build_transaction_to_send_eth_with_nonce(
        &self,
        from: User,
        to: User,
        amount: u128,
        nonce: u64,
    ) -> OpTransactionSigned {
        let txn = TransactionBuilder::default()
            .signer(self.signer(from))
            .chain_id(self.provider.chain_spec().chain_id())
            .to(self.address(to))
            .nonce(nonce)
            .value(amount)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559()
            .as_eip1559()
            .unwrap()
            .clone();

        OpTransactionSigned::Eip1559(txn)
    }

    async fn send_flashblock(&self, flashblock: Flashblock) {
        self.node
            .send_flashblock(flashblock)
            .await
            .expect("flashblocks channel should accept payload");
        sleep(Duration::from_millis(SLEEP_TIME)).await;
    }

    async fn new_canonical_block_without_processing(
        &mut self,
        user_transactions: Vec<OpTransactionSigned>,
    ) -> RecoveredBlock<OpBlock> {
        let previous_tip =
            self.provider.best_block_number().expect("able to read best block number");
        let txs: Vec<Bytes> =
            user_transactions.into_iter().map(|tx| tx.encoded_2718().into()).collect();
        self.node.build_block_from_transactions(txs).await.expect("able to build block");
        let target_block_number = previous_tip + 1;

        let block = self
            .provider
            .block(BlockHashOrNumber::Number(target_block_number))
            .expect("able to load block")
            .expect("new canonical block should be available after building payload");

        block.try_into_recovered().expect("able to recover newly built block")
    }

    async fn new_canonical_block(&mut self, user_transactions: Vec<OpTransactionSigned>) {
        let block = self.new_canonical_block_without_processing(user_transactions).await;
        self.flashblocks.on_canonical_block_received(block);
        sleep(Duration::from_millis(SLEEP_TIME)).await;
    }
}

struct FlashblockBuilder<'a> {
    transactions: Vec<Bytes>,
    receipts: Option<HashMap<B256, OpReceipt>>,
    harness: &'a TestHarness,
    canonical_block_number: Option<BlockNumber>,
    index: u64,
}

impl<'a> FlashblockBuilder<'a> {
    fn new_base(harness: &'a TestHarness) -> Self {
        Self {
            canonical_block_number: None,
            transactions: vec![L1_BLOCK_INFO_DEPOSIT_TX.clone()],
            receipts: Some({
                let mut receipts = alloy_primitives::map::HashMap::default();
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
            }),
            index: 0,
            harness,
        }
    }
    fn new(harness: &'a TestHarness, index: u64) -> Self {
        Self {
            canonical_block_number: None,
            transactions: Vec::new(),
            receipts: Some(HashMap::default()),
            harness,
            index,
        }
    }

    fn with_receipts(&mut self, receipts: Option<HashMap<B256, OpReceipt>>) -> &mut Self {
        self.receipts = receipts;
        self
    }

    fn with_transactions(&mut self, transactions: Vec<OpTransactionSigned>) -> &mut Self {
        assert_ne!(self.index, 0, "Cannot set txns for initial flashblock");
        self.transactions.clear();

        let mut cumulative_gas_used = 0;
        for txn in transactions.iter() {
            cumulative_gas_used = cumulative_gas_used + txn.gas_limit();
            self.transactions.push(txn.encoded_2718().into());
            if let Some(ref mut receipts) = self.receipts {
                receipts.insert(
                    txn.hash().clone(),
                    OpReceipt::Eip1559(Receipt {
                        status: true.into(),
                        cumulative_gas_used,
                        logs: vec![],
                    }),
                );
            }
        }
        self
    }

    fn with_canonical_block_number(&mut self, num: BlockNumber) -> &mut Self {
        self.canonical_block_number = Some(num);
        self
    }

    fn build(&self) -> Flashblock {
        let current_block = self.harness.node.latest_block();
        let canonical_block_num =
            self.canonical_block_number.unwrap_or_else(|| current_block.number) + 1;

        let base = if self.index == 0 {
            Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: current_block.hash(),
                parent_hash: current_block.hash(),
                fee_recipient: Address::random(),
                prev_randao: B256::random(),
                block_number: canonical_block_num,
                gas_limit: current_block.gas_limit,
                timestamp: current_block.timestamp + 2,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(100),
            })
        } else {
            None
        };

        Flashblock {
            payload_id: PayloadId::default(),
            index: self.index,
            base,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                block_hash: B256::default(),
                gas_used: 0,
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
                transactions: self.transactions.clone(),
                blob_gas_used: Default::default(),
            },
            metadata: Metadata { block_number: canonical_block_num },
        }
    }
}

#[tokio::test]
async fn test_state_overrides_persisted_across_flashblocks() {
    let test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("block is built")
            .transactions
            .len(),
        1
    );

    assert!(test.flashblocks.get_pending_blocks().get_state_overrides().is_some());
    assert!(
        !test
            .flashblocks
            .get_pending_blocks()
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice))
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    let pending = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(pending.is_some());
    let pending = pending.unwrap();
    assert_eq!(pending.transactions.len(), 2);

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 100_000)
    );

    test.send_flashblock(FlashblockBuilder::new(&test, 2).build()).await;

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution in flashblock index 1");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 100_000)
    );
}

#[tokio::test]
async fn test_state_overrides_persisted_across_blocks() {
    let test = TestHarness::new().await;

    let initial_base = FlashblockBuilder::new_base(&test).build();
    let initial_block_number = initial_base.metadata.block_number;
    test.send_flashblock(initial_base).await;
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("block is built")
            .transactions
            .len(),
        1
    );

    assert!(test.flashblocks.get_pending_blocks().get_state_overrides().is_some());
    assert!(
        !test
            .flashblocks
            .get_pending_blocks()
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice))
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    let pending = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(pending.is_some());
    let pending = pending.unwrap();
    assert_eq!(pending.transactions.len(), 2);

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 100_000)
    );

    test.send_flashblock(
        FlashblockBuilder::new_base(&test)
            .with_canonical_block_number(initial_block_number)
            .build(),
    )
    .await;

    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("block is built")
            .transactions
            .len(),
        1
    );
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("block is built")
            .header
            .number,
        initial_block_number + 1
    );

    assert!(test.flashblocks.get_pending_blocks().get_state_overrides().is_some());
    assert!(
        test.flashblocks
            .get_pending_blocks()
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice))
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_canonical_block_number(initial_block_number)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 200_000)
    );
}

#[tokio::test]
async fn test_only_current_pending_state_cleared_upon_canonical_block_reorg() {
    let mut test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("block is built")
            .transactions
            .len(),
        1
    );
    assert!(test.flashblocks.get_pending_blocks().get_state_overrides().is_some());
    assert!(
        !test
            .flashblocks
            .get_pending_blocks()
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice))
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build(),
    )
    .await;
    let pending = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(pending.is_some());
    let pending = pending.unwrap();
    assert_eq!(pending.transactions.len(), 2);

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 100_000)
    );

    test.send_flashblock(FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build())
        .await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_canonical_block_number(1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build(),
    )
    .await;
    let pending = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(pending.is_some());
    let pending = pending.unwrap();
    assert_eq!(pending.transactions.len(), 2);

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 200_000)
    );

    test.new_canonical_block(vec![test.build_transaction_to_send_eth_with_nonce(
        User::Alice,
        User::Bob,
        100,
        0,
    )])
    .await;

    let pending = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(pending.is_some());
    let pending = pending.unwrap();
    assert_eq!(pending.transactions.len(), 2);

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution");

    assert!(overrides.get(&test.address(User::Alice)).is_some());
    assert_eq!(
        overrides
            .get(&test.address(User::Bob))
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(User::Bob, 100_000)
    );
}

#[tokio::test]
async fn test_nonce_uses_pending_canon_block_instead_of_latest() {
    // Test for race condition when a canon block comes in but user
    // requests their nonce prior to the StateProcessor processing the canon block
    // causing it to return an n+1 nonce instead of n
    // because underlying reth node `latest` block is already updated, but
    // relevant pending state has not been cleared yet
    let mut test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100,
            )])
            .build(),
    )
    .await;

    let pending_nonce =
        test.provider.basic_account(&test.address(User::Alice)).unwrap().unwrap().nonce
            + test
                .flashblocks
                .get_pending_blocks()
                .get_transaction_count(test.address(User::Alice))
                .to::<u64>();
    assert_eq!(pending_nonce, 1);

    test.new_canonical_block_without_processing(vec![
        test.build_transaction_to_send_eth_with_nonce(User::Alice, User::Bob, 100, 0),
    ])
    .await;

    let pending_nonce =
        test.provider.basic_account(&test.address(User::Alice)).unwrap().unwrap().nonce
            + test
                .flashblocks
                .get_pending_blocks()
                .get_transaction_count(test.address(User::Alice))
                .to::<u64>();

    // This is 2, because canon block has reached the underlying chain
    // but the StateProcessor hasn't processed it
    // so pending nonce is effectively double-counting the same transaction, leading to a nonce of 2
    assert_eq!(pending_nonce, 2);

    // On the RPC level, we correctly return 1 because we
    // use the pending canon block instead of the latest block when fetching
    // onchain nonce count to compute
    // pending_nonce = onchain_nonce + pending_txn_count
    let canon_block = test.flashblocks.get_pending_blocks().get_canonical_block_number();
    let canon_state_provider = test.provider.state_by_block_number_or_tag(canon_block).unwrap();
    let canon_nonce =
        canon_state_provider.account_nonce(&test.address(User::Alice)).unwrap().unwrap();
    let pending_nonce = canon_nonce
        + test
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(test.address(User::Alice))
            .to::<u64>();
    assert_eq!(pending_nonce, 1);
}

#[tokio::test]
async fn test_metadata_receipts_are_optional() {
    // Test to ensure that receipts are optional in the metadata
    // and deposit receipts return None for nonce until the canonical block is processed
    let test = TestHarness::new().await;

    // Send a flashblock with no receipts (only deposit transaction)
    test.send_flashblock(FlashblockBuilder::new_base(&test).with_receipts(None).build()).await;

    // Verify the block was created with the deposit transaction
    let pending_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("block should be created");
    assert_eq!(pending_block.transactions.len(), 1);

    // Check that the deposit transaction has the correct nonce
    let deposit_tx = &pending_block.transactions.as_transactions().unwrap()[0];
    assert_eq!(
        deposit_tx.deposit_nonce,
        Some(0),
        "deposit_nonce should be available even when no receipts"
    );
}

#[tokio::test]
async fn test_flashblock_for_new_canonical_block_clears_older_flashblocks_if_non_zero_index() {
    let test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let current_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("should be a block");

    assert_eq!(current_block.header().number, 1);
    assert_eq!(current_block.transactions.len(), 1);

    test.send_flashblock(FlashblockBuilder::new(&test, 1).with_canonical_block_number(100).build())
        .await;

    let current_block = test.flashblocks.get_pending_blocks().get_block(true);
    assert!(current_block.is_none());
}

#[tokio::test]
async fn test_flashblock_for_new_canonical_block_works_if_sequential() {
    let test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let current_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("should be a block");

    assert_eq!(current_block.header().number, 1);
    assert_eq!(current_block.transactions.len(), 1);

    test.send_flashblock(FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build())
        .await;

    let current_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("should be a block");

    assert_eq!(current_block.header().number, 2);
    assert_eq!(current_block.transactions.len(), 1);
}

#[tokio::test]
async fn test_non_sequential_payload_clears_pending_state() {
    let test = TestHarness::new().await;

    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    // Just the block info transaction
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("should be set")
            .transactions
            .len(),
        1
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 3)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100,
            )])
            .build(),
    )
    .await;

    assert_eq!(test.flashblocks.get_pending_blocks().is_none(), true);
}

#[tokio::test]
async fn test_duplicate_flashblock_ignored() {
    let test = TestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let fb = FlashblockBuilder::new(&test, 1)
        .with_transactions(vec![test.build_transaction_to_send_eth(
            User::Alice,
            User::Bob,
            100_000,
        )])
        .build();

    test.send_flashblock(fb.clone()).await;
    let block = test.flashblocks.get_pending_blocks().get_block(true);

    test.send_flashblock(fb.clone()).await;
    let block_two = test.flashblocks.get_pending_blocks().get_block(true);

    assert_eq!(block, block_two);
}

/// Verifies that eth_call targeting pending block sees flashblock state changes.
///
/// This test catches database layering bugs where pending state from flashblocks
/// isn't visible to RPC callers. After a flashblock transfers ETH to Bob, an
/// eth_call simulating a transfer FROM Bob should succeed because Bob now has
/// more funds from the flashblock.
#[tokio::test]
async fn test_eth_call_sees_flashblock_state_changes() {
    use alloy_eips::BlockNumberOrTag;
    use alloy_provider::Provider;
    use alloy_rpc_types_eth::TransactionInput;
    use op_alloy_rpc_types::OpTransactionRequest;

    let test = TestHarness::new().await;
    let provider = test.node.provider();

    let bob_address = test.address(User::Bob);
    let charlie_address = test.address(User::Charlie);

    // Get Bob's canonical balance to calculate a transfer amount that exceeds it
    let canonical_balance = provider.get_balance(bob_address).await.unwrap();

    // Send base flashblock
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    // Flashblock 1: Alice sends a large amount to Bob
    let transfer_to_bob = 1_000_000_000_000_000_000u128; // 1 ETH
    let tx =
        test.build_transaction_to_send_eth_with_nonce(User::Alice, User::Bob, transfer_to_bob, 0);
    test.send_flashblock(FlashblockBuilder::new(&test, 1).with_transactions(vec![tx]).build())
        .await;

    // Verify via state overrides that Bob received the funds
    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("state overrides should exist after flashblock execution");
    let bob_override = overrides.get(&bob_address).expect("Bob should have a state override");
    let bob_pending_balance = bob_override.balance.expect("Bob's balance override should be set");
    assert_eq!(
        bob_pending_balance,
        canonical_balance + U256::from(transfer_to_bob),
        "State override should show Bob's increased balance"
    );

    // Now the key test: eth_call from Bob should see this pending balance.
    // Try to transfer more than Bob's canonical balance (but less than pending).
    // This would fail if eth_call can't see the pending state.
    let transfer_amount = canonical_balance + U256::from(100_000u64);
    let call_request = OpTransactionRequest::default()
        .from(bob_address)
        .to(charlie_address)
        .value(transfer_amount)
        .gas_limit(21_000)
        .input(TransactionInput::default());

    let result = provider.call(call_request).block(BlockNumberOrTag::Pending.into()).await;
    assert!(
        result.is_ok(),
        "eth_call from Bob should succeed because pending state shows increased balance. \
         If this fails, eth_call may not be seeing flashblock state changes. Error: {:?}",
        result.err()
    );
}

/// Verifies that transactions in flashblock N+1 can see state changes from flashblock N.
///
/// This test catches database layering bugs where writes from earlier flashblocks
/// aren't visible to later flashblock execution. The key is that flashblock 2's
/// transaction uses nonce=1, which only succeeds if the execution layer sees
/// flashblock 1's transaction (which used nonce=0).
#[tokio::test]
async fn test_sequential_nonces_across_flashblocks() {
    let test = TestHarness::new().await;

    // Send base flashblock
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    // Flashblock 1: Alice sends to Bob with nonce 0
    let tx_nonce_0 = test.build_transaction_to_send_eth_with_nonce(User::Alice, User::Bob, 1000, 0);
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1).with_transactions(vec![tx_nonce_0]).build(),
    )
    .await;

    // Verify flashblock 1 was processed - Alice's pending nonce should now be 1
    let alice_state = test.account_state(User::Alice);
    assert_eq!(alice_state.nonce, 1, "After flashblock 1, Alice's pending nonce should be 1");

    // Flashblock 2: Alice sends to Charlie with nonce 1
    // This will FAIL if the execution layer can't see flashblock 1's state change
    let tx_nonce_1 =
        test.build_transaction_to_send_eth_with_nonce(User::Alice, User::Charlie, 2000, 1);
    test.send_flashblock(
        FlashblockBuilder::new(&test, 2).with_transactions(vec![tx_nonce_1]).build(),
    )
    .await;

    // Verify flashblock 2 was processed - Alice's pending nonce should now be 2
    let alice_state_after = test.account_state(User::Alice);
    assert_eq!(
        alice_state_after.nonce, 2,
        "After flashblock 2, Alice's pending nonce should be 2. \
         If this fails, the database layering may be preventing flashblock 2 \
         from seeing flashblock 1's state changes."
    );

    // Also verify Bob and Charlie received their funds
    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("state overrides should exist");

    assert!(
        overrides.get(&test.address(User::Bob)).is_some(),
        "Bob should have received funds from flashblock 1"
    );
    assert!(
        overrides.get(&test.address(User::Charlie)).is_some(),
        "Charlie should have received funds from flashblock 2"
    );
}

#[tokio::test]
async fn test_progress_canonical_blocks_without_flashblocks() {
    let mut test = TestHarness::new().await;

    let genesis_block = test.node.latest_block();
    assert_eq!(genesis_block.number, 0);
    assert_eq!(genesis_block.transaction_count(), 0);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.new_canonical_block(vec![test.build_transaction_to_send_eth(User::Alice, User::Bob, 100)])
        .await;

    let block_one = test.node.latest_block();
    assert_eq!(block_one.number, 1);
    assert_eq!(block_one.transaction_count(), 2);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.new_canonical_block(vec![
        test.build_transaction_to_send_eth(User::Bob, User::Charlie, 100),
        test.build_transaction_to_send_eth(User::Charlie, User::Alice, 1000),
    ])
    .await;

    let block_two = test.node.latest_block();
    assert_eq!(block_two.number, 2);
    assert_eq!(block_two.transaction_count(), 3);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());
}
