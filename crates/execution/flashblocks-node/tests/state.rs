//! Integration tests that stress Flashblocks state handling.

use std::time::{Duration, Instant};

use alloy_consensus::{Header, Sealed};
use alloy_network::BlockResponse;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use base_flashblocks::{
    FlashblocksAPI, FlashblocksReceiver, MAX_FLASHBLOCKS_PER_PAYLOAD, PendingBlocks,
    PendingBlocksAPI, PendingBlocksBuilder,
};
use base_flashblocks_node::test_harness::{FlashblockBuilder, FlashblocksBuilderTestHarness};
use base_test_utils::Account;
use reth_provider::{AccountReader, StateProviderFactory};
use tokio::time::sleep;

#[tokio::test]
async fn test_state_overrides_persisted_across_flashblocks() {
    let test = FlashblocksBuilderTestHarness::new().await;

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
            .contains_key(&Account::Alice.address())
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
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

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 100_000)
    );

    test.send_flashblock(FlashblockBuilder::new(&test, 2).build()).await;

    let overrides = test
        .flashblocks
        .get_pending_blocks()
        .get_state_overrides()
        .expect("should be set from txn execution in flashblock index 1");

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 100_000)
    );
}

#[tokio::test]
async fn test_state_overrides_persisted_across_blocks() {
    let test = FlashblocksBuilderTestHarness::new().await;

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
            .contains_key(&Account::Alice.address())
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
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

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 100_000)
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
            .contains_key(&Account::Alice.address())
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_canonical_block_number(initial_block_number)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
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

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 200_000)
    );
}

#[tokio::test]
async fn test_only_current_pending_state_cleared_upon_canonical_block_reorg() {
    let mut test = FlashblocksBuilderTestHarness::new().await;

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
            .contains_key(&Account::Alice.address())
    );

    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
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

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 100_000)
    );

    test.send_flashblock(FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build())
        .await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_canonical_block_number(1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
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

    assert!(overrides.contains_key(&Account::Alice.address()));
    assert_eq!(
        overrides
            .get(&Account::Bob.address())
            .expect("should be set as txn receiver")
            .balance
            .expect("should be changed due to receiving funds"),
        test.expected_pending_balance(Account::Bob, 200_000)
    );

    test.new_canonical_block(vec![test.build_transaction_to_send_eth_with_nonce(
        Account::Alice,
        Account::Bob,
        100,
        0,
    )])
    .await;

    assert!(
        test.flashblocks.get_pending_blocks().get_block(true).is_none(),
        "a real overlapping transaction mismatch must clear pending state"
    );
}

#[tokio::test]
async fn test_nonce_uses_pending_canon_block_instead_of_latest() {
    // Test for race condition when a canon block comes in but user
    // requests their nonce prior to the StateProcessor processing the canon block
    // causing it to return an n+1 nonce instead of n
    // because underlying reth node `latest` block is already updated, but
    // relevant pending state has not been cleared yet
    let mut test = FlashblocksBuilderTestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                100,
            )])
            .build(),
    )
    .await;

    let pending_nonce = test
        .provider
        .latest()
        .unwrap()
        .basic_account(&Account::Alice.address())
        .unwrap()
        .unwrap()
        .nonce
        + test
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(Account::Alice.address())
            .to::<u64>();
    assert_eq!(pending_nonce, 1);

    test.new_canonical_block_without_processing(vec![
        test.build_transaction_to_send_eth_with_nonce(Account::Alice, Account::Bob, 100, 0),
    ])
    .await;

    let pending_nonce = test
        .provider
        .latest()
        .unwrap()
        .basic_account(&Account::Alice.address())
        .unwrap()
        .unwrap()
        .nonce
        + test
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(Account::Alice.address())
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
        canon_state_provider.account_nonce(&Account::Alice.address()).unwrap().unwrap();
    let pending_nonce = canon_nonce
        + test
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(Account::Alice.address())
            .to::<u64>();
    assert_eq!(pending_nonce, 1);
}

#[tokio::test]
async fn test_metadata_receipts_are_optional() {
    // Test to ensure that receipts are optional in the metadata
    // and deposit receipts return None for nonce until the canonical block is processed
    let test = FlashblocksBuilderTestHarness::new().await;

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
async fn test_nonzero_unrepresented_block_does_not_clear_pending() {
    let test = FlashblocksBuilderTestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let current_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("should be a block");

    assert_eq!(current_block.header().number, 1);
    assert_eq!(current_block.transactions.len(), 1);

    test.send_flashblock(FlashblockBuilder::new(&test, 1).with_canonical_block_number(100).build())
        .await;

    let current_block =
        test.flashblocks.get_pending_blocks().get_block(true).expect("pending remains published");
    assert_eq!(current_block.header().number, 1);
    assert_eq!(current_block.transactions.len(), 1);
}

#[tokio::test]
async fn test_flashblock_for_new_canonical_block_works_if_sequential() {
    let test = FlashblocksBuilderTestHarness::new().await;

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
    let test = FlashblocksBuilderTestHarness::new().await;

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
                Account::Alice,
                Account::Bob,
                100,
            )])
            .build(),
    )
    .await;

    assert!(test.flashblocks.get_pending_blocks().is_none());
}

#[tokio::test]
async fn test_duplicate_flashblock_ignored() {
    let test = FlashblocksBuilderTestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let fb = FlashblockBuilder::new(&test, 1)
        .with_transactions(vec![test.build_transaction_to_send_eth(
            Account::Alice,
            Account::Bob,
            100_000,
        )])
        .build();

    test.send_flashblock(fb.clone()).await;
    let block = test.flashblocks.get_pending_blocks().get_block(true);

    test.send_flashblock(fb.clone()).await;
    let block_two = test.flashblocks.get_pending_blocks().get_block(true);

    assert_eq!(block, block_two);
}

/// Verifies that `eth_call` targeting pending block sees flashblock state changes.
///
/// This test catches database layering bugs where pending state from flashblocks
/// isn't visible to RPC callers. After a flashblock transfers ETH to Bob, an
/// `eth_call` simulating a transfer FROM Bob should succeed because Bob now has
/// more funds from the flashblock.
#[tokio::test]
async fn test_eth_call_sees_flashblock_state_changes() {
    use alloy_eips::BlockNumberOrTag;
    use alloy_provider::Provider;
    use alloy_rpc_types_eth::TransactionInput;
    use base_common_rpc_types::BaseTransactionRequest;

    let test = FlashblocksBuilderTestHarness::new().await;
    let provider = test.node.provider();

    let bob_address = Account::Bob.address();
    let charlie_address = Account::Charlie.address();

    // Get Bob's canonical balance to calculate a transfer amount that exceeds it
    let canonical_balance = provider.get_balance(bob_address).await.unwrap();

    // Send base flashblock
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    // Flashblock 1: Alice sends a large amount to Bob
    let transfer_to_bob = 1_000_000_000_000_000_000u128; // 1 ETH
    let tx = test.build_transaction_to_send_eth_with_nonce(
        Account::Alice,
        Account::Bob,
        transfer_to_bob,
        0,
    );
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
    let call_request = BaseTransactionRequest::default()
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
    let test = FlashblocksBuilderTestHarness::new().await;

    // Send base flashblock
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    // Flashblock 1: Alice sends to Bob with nonce 0
    let tx_nonce_0 =
        test.build_transaction_to_send_eth_with_nonce(Account::Alice, Account::Bob, 1000, 0);
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1).with_transactions(vec![tx_nonce_0]).build(),
    )
    .await;

    // Verify flashblock 1 was processed - Alice's pending nonce should now be 1
    let alice_state = test.account_state(Account::Alice);
    assert_eq!(alice_state.nonce, 1, "After flashblock 1, Alice's pending nonce should be 1");

    // Flashblock 2: Alice sends to Charlie with nonce 1
    // This will FAIL if the execution layer can't see flashblock 1's state change
    let tx_nonce_1 =
        test.build_transaction_to_send_eth_with_nonce(Account::Alice, Account::Charlie, 2000, 1);
    test.send_flashblock(
        FlashblockBuilder::new(&test, 2).with_transactions(vec![tx_nonce_1]).build(),
    )
    .await;

    // Verify flashblock 2 was processed - Alice's pending nonce should now be 2
    let alice_state_after = test.account_state(Account::Alice);
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
        overrides.contains_key(&Account::Bob.address()),
        "Bob should have received funds from flashblock 1"
    );
    assert!(
        overrides.contains_key(&Account::Charlie.address()),
        "Charlie should have received funds from flashblock 2"
    );
}

#[tokio::test]
async fn test_cached_wrong_parent_sequence_does_not_contaminate_fresh_pending() {
    let mut test = FlashblocksBuilderTestHarness::new().await;

    let transfer_amount = 100_000u128;

    // Cache a base flashblock for block 2 (needs canonical block 1).
    test.send_flashblock(FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build())
        .await;
    assert!(test.flashblocks.get_pending_blocks().is_none());

    // Also cache a second flashblock (index 1) with a transaction.
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_canonical_block_number(1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                transfer_amount,
            )])
            .build(),
    )
    .await;
    assert!(test.flashblocks.get_pending_blocks().is_none());

    let block_one = test.new_canonical_block_without_processing(vec![]).await;
    let block_one_number = block_one.number;
    let block_one_hash = block_one.hash();
    let resume = FlashblockBuilder::new_base(&test).build();

    test.flashblocks.on_canonical_block_received(block_one);
    test.flashblocks.on_flashblock_received(resume);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == block_one_number + 1
                    && pending.parent_hash() == block_one_hash
                    && pending.pending_transaction_count() == 1
            })
        },
        "old-parent cached deltas must not contaminate the fresh sequence",
    )
    .await;
}

#[tokio::test]
async fn test_flashblock_far_ahead_of_canonical_not_cached() {
    let test = FlashblocksBuilderTestHarness::new().await;

    // Send a flashblock targeting a block far in the future (canonical_block_number=100).
    // This is more than MAX_CACHE_AHEAD_BLOCKS (5) ahead of genesis, so it should NOT
    // be cached and pending state should remain empty.
    test.send_flashblock(
        FlashblockBuilder::new_base(&test).with_canonical_block_number(100).build(),
    )
    .await;

    assert!(
        test.flashblocks.get_pending_blocks().is_none(),
        "flashblock too far ahead should not be cached or produce pending state"
    );
}

#[tokio::test]
async fn test_progress_canonical_blocks_without_flashblocks() {
    let mut test = FlashblocksBuilderTestHarness::new().await;

    let genesis_block = test.node.latest_block();
    assert_eq!(genesis_block.number, 0);
    assert_eq!(genesis_block.transaction_count(), 0);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.new_canonical_block(vec![test.build_transaction_to_send_eth(
        Account::Alice,
        Account::Bob,
        100,
    )])
    .await;

    let block_one = test.node.latest_block();
    assert_eq!(block_one.number, 1);
    assert_eq!(block_one.transaction_count(), 1);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.new_canonical_block(vec![
        test.build_transaction_to_send_eth(Account::Bob, Account::Charlie, 100),
        test.build_transaction_to_send_eth(Account::Charlie, Account::Alice, 1000),
    ])
    .await;

    let block_two = test.node.latest_block();
    assert_eq!(block_two.number, 2);
    assert_eq!(block_two.transaction_count(), 2);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());
}

#[tokio::test]
async fn test_bundle_state_published_for_pending_metering() {
    let test = FlashblocksBuilderTestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    let pending_blocks = test.flashblocks.get_pending_blocks();
    let bundle_state =
        pending_blocks.as_ref().expect("pending blocks should exist").get_bundle_state();

    assert!(
        bundle_state.account(&Account::Alice.address()).is_some(),
        "pending bundle_state must include Alice for bundle metering consumers"
    );
    assert!(
        bundle_state.account(&Account::Bob.address()).is_some(),
        "pending bundle_state must include Bob for bundle metering consumers"
    );
}

#[tokio::test]
async fn test_same_block_append_refreshes_pending_header() {
    let test = FlashblocksBuilderTestHarness::new().await;

    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    let after_first_append =
        test.flashblocks.get_pending_blocks().get_block(true).expect("block should exist");
    let first_txs = after_first_append.transactions.len();
    let first_transactions_root = after_first_append.header.transactions_root;

    test.send_flashblock(
        FlashblockBuilder::new(&test, 2)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Charlie,
                200_000,
            )])
            .build(),
    )
    .await;

    let after_second_append =
        test.flashblocks.get_pending_blocks().get_block(true).expect("block should exist");

    assert_eq!(after_second_append.header.number, after_first_append.header.number);
    assert_eq!(after_second_append.transactions.len(), first_txs + 1);
    assert!(
        after_second_append.header.transactions_root != first_transactions_root,
        "same-block append must publish a fresh header with updated transactions_root"
    );
}

fn dummy_flashblock(block_number: u64, parent_hash: B256) -> Flashblock {
    Flashblock {
        payload_id: Default::default(),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: B256::ZERO,
            parent_hash,
            fee_recipient: Default::default(),
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 30_000_000,
            timestamp: 1_700_000_000 + block_number,
            extra_data: Default::default(),
            base_fee_per_gas: U256::from(1_000_000_000u64),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1::default(),
        metadata: Metadata::new(block_number),
    }
}

fn pending_spanning(
    earliest: u64,
    latest: u64,
    parent_hash: B256,
    suffix: Flashblock,
) -> PendingBlocks {
    let mut builder = PendingBlocksBuilder::new();
    builder.with_header(Sealed::new_unchecked(
        Header { number: earliest, parent_hash, ..Default::default() },
        B256::ZERO,
    ));
    builder.with_header(Sealed::new_unchecked(
        Header { number: latest, parent_hash: B256::ZERO, ..Default::default() },
        B256::ZERO,
    ));
    builder.with_flashblocks([dummy_flashblock(earliest, parent_hash), suffix]);
    builder.build().expect("pending snapshot should build")
}

async fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool, failure: &str) {
    let start = Instant::now();
    loop {
        if check() {
            return;
        }
        assert!(start.elapsed() <= timeout, "{failure}");
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn test_matching_canonical_rebases_pending_onto_canonical_tip() {
    let mut test = FlashblocksBuilderTestHarness::new().await;
    let genesis_hash = test.node.latest_block().hash();
    let block_one = test.new_canonical_block_without_processing(vec![]).await;
    let suffix = FlashblockBuilder::new_base(&test).build();

    test.flashblocks.set_pending_blocks_for_testing(Some(pending_spanning(
        1,
        2,
        genesis_hash,
        suffix,
    )));
    test.flashblocks.on_canonical_block_received(block_one.clone());

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 2 && pending.parent_hash() == block_one.hash()
            })
        },
        "pending snapshot should rebase onto canonical block 1",
    )
    .await;
}

#[tokio::test]
async fn test_wrong_parent_rebase_clears_pending_until_matching_resume() {
    let mut test = FlashblocksBuilderTestHarness::new().await;
    let genesis_hash = test.node.latest_block().hash();
    let block_one = test.new_canonical_block_without_processing(vec![]).await;
    let mut wrong_suffix = FlashblockBuilder::new_base(&test).build();
    wrong_suffix.base.as_mut().expect("base flashblock has a base payload").parent_hash =
        B256::repeat_byte(0x24);

    test.flashblocks.set_pending_blocks_for_testing(Some(pending_spanning(
        1,
        2,
        genesis_hash,
        wrong_suffix,
    )));
    test.flashblocks.on_canonical_block_received(block_one.clone());

    wait_until(
        Duration::from_secs(5),
        || test.flashblocks.get_pending_blocks().is_none(),
        "wrong-parent suffix must clear pending during rebase",
    )
    .await;

    test.flashblocks.on_flashblock_received(FlashblockBuilder::new_base(&test).build());
    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 2 && pending.parent_hash() == block_one.hash()
            })
        },
        "recovery must resume from a matching current base",
    )
    .await;
}

#[tokio::test]
async fn test_untracked_older_canonical_does_not_false_reorg() {
    let mut test = FlashblocksBuilderTestHarness::new().await;
    let genesis = test.node.latest_block();
    let block_one = test.new_canonical_block_without_processing(vec![]).await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    test.flashblocks.on_canonical_block_received(genesis);
    sleep(Duration::from_millis(100)).await;

    let pending = test.flashblocks.get_pending_blocks();
    let pending =
        pending.as_ref().expect("untracked canonical must not trigger an empty-vector reorg");
    assert_eq!(pending.earliest_block_number(), 2);
    assert_eq!(pending.latest_block_number(), 2);
    assert_eq!(pending.parent_hash(), block_one.hash());
}

#[tokio::test]
async fn test_wrong_parent_payload_does_not_contaminate_accepted_payload() {
    let test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let mut wrong_parent = FlashblockBuilder::new_base(&test).build();
    wrong_parent.payload_id = PayloadId::new([1; 8]);
    wrong_parent.base.as_mut().expect("base flashblock has a base payload").parent_hash =
        B256::repeat_byte(0x42);
    let mut second_wrong_parent = FlashblockBuilder::new_base(&test).build();
    second_wrong_parent.payload_id = PayloadId::new([2; 8]);
    second_wrong_parent.base.as_mut().expect("base flashblock has a base payload").parent_hash =
        B256::repeat_byte(0x43);
    let mut rejected_delta = FlashblockBuilder::new(&test, 1)
        .with_transactions(vec![test.build_transaction_to_send_eth(
            Account::Alice,
            Account::Bob,
            50_000,
        )])
        .build();
    rejected_delta.payload_id = PayloadId::new([1; 8]);

    test.flashblocks.on_flashblock_received(wrong_parent);
    test.flashblocks.on_flashblock_received(second_wrong_parent);
    test.flashblocks.on_flashblock_received(rejected_delta);
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .as_ref()
            .expect("healthy pending remains published")
            .pending_transaction_count(),
        1
    );

    test.flashblocks.on_flashblock_received(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                100_000,
            )])
            .build(),
    );

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks
                .get_pending_blocks()
                .as_ref()
                .is_some_and(|pending| pending.pending_transaction_count() == 2)
        },
        "a wrong build must not quarantine deltas from the accepted payload",
    )
    .await;
}

#[tokio::test]
async fn test_current_base_from_new_payload_replaces_pending() {
    let test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let replacement_payload = PayloadId::new([9; 8]);
    let mut replacement = FlashblockBuilder::new_base(&test).build();
    replacement.payload_id = replacement_payload;
    test.flashblocks.on_flashblock_received(replacement);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.latest_payload_id() == replacement_payload
                    && pending.latest_flashblock_index() == 0
            })
        },
        "a new current payload base must replace abandoned pending state",
    )
    .await;

    let mut replacement_delta = FlashblockBuilder::new(&test, 1)
        .with_transactions(vec![test.build_transaction_to_send_eth(
            Account::Alice,
            Account::Bob,
            100_000,
        )])
        .build();
    replacement_delta.payload_id = replacement_payload;
    test.flashblocks.on_flashblock_received(replacement_delta);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks
                .get_pending_blocks()
                .as_ref()
                .is_some_and(|pending| pending.pending_transaction_count() == 2)
        },
        "deltas from the replacement payload must extend pending state",
    )
    .await;
}

#[tokio::test]
async fn test_same_payload_base_retry_does_not_clear_deltas() {
    let test = FlashblocksBuilderTestHarness::new().await;
    let base = FlashblockBuilder::new_base(&test).build();
    test.send_flashblock(base.clone()).await;
    test.send_flashblock(
        FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                Account::Alice,
                Account::Bob,
                100_000,
            )])
            .build(),
    )
    .await;

    test.flashblocks.on_flashblock_received(base);
    test.send_flashblock(FlashblockBuilder::new(&test, 2).build()).await;
    let pending = test.flashblocks.get_pending_blocks();
    let pending = pending.as_ref().expect("pending state remains published");
    assert_eq!(pending.latest_flashblock_index(), 2);
    assert_eq!(pending.pending_transaction_count(), 2);
}

#[tokio::test]
async fn test_conflicting_live_retransmissions_clear_pending() {
    let test = FlashblocksBuilderTestHarness::new().await;
    let base = FlashblockBuilder::new_base(&test).build();
    test.send_flashblock(base.clone()).await;

    let mut conflicting_base = base.clone();
    conflicting_base.diff.state_root = B256::repeat_byte(0x11);
    test.send_flashblock(conflicting_base).await;
    assert!(test.flashblocks.get_pending_blocks().is_none());

    test.send_flashblock(base).await;
    let delta = FlashblockBuilder::new(&test, 1).build();
    test.send_flashblock(delta.clone()).await;
    let mut conflicting_delta = delta;
    conflicting_delta.diff.state_root = B256::repeat_byte(0x22);
    test.send_flashblock(conflicting_delta).await;
    assert!(test.flashblocks.get_pending_blocks().is_none());
}

#[tokio::test]
async fn test_new_payload_replaces_latest_block_of_pending_chain() {
    let test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let block_two = FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build();
    test.send_flashblock(block_two.clone()).await;

    let replacement_payload = PayloadId::new([8; 8]);
    let mut replacement = block_two;
    replacement.payload_id = replacement_payload;
    test.flashblocks.on_flashblock_received(replacement);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 1
                    && pending.latest_block_number() == 2
                    && pending.latest_payload_id() == replacement_payload
            })
        },
        "a replacement payload must rebuild the latest pending suffix",
    )
    .await;

    let mut replacement_delta =
        FlashblockBuilder::new(&test, 1).with_canonical_block_number(1).build();
    replacement_delta.payload_id = replacement_payload;
    test.flashblocks.on_flashblock_received(replacement_delta);
    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks
                .get_pending_blocks()
                .as_ref()
                .is_some_and(|pending| pending.latest_flashblock_index() == 1)
        },
        "the replacement payload must accept its following deltas",
    )
    .await;
}

#[tokio::test]
async fn test_new_payload_replaces_earlier_block_and_discards_suffix() {
    let test = FlashblocksBuilderTestHarness::new().await;
    let block_one = FlashblockBuilder::new_base(&test).build();
    test.send_flashblock(block_one.clone()).await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build())
        .await;

    let replacement_payload = PayloadId::new([7; 8]);
    let mut replacement = block_one;
    replacement.payload_id = replacement_payload;
    test.flashblocks.on_flashblock_received(replacement);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 1
                    && pending.latest_block_number() == 1
                    && pending.latest_payload_id() == replacement_payload
            })
        },
        "replacing an earlier payload must discard its abandoned pending suffix",
    )
    .await;
}

#[tokio::test]
async fn test_wrong_internal_parent_does_not_extend_pending_sequence() {
    let test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let mut wrong_base = FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build();
    wrong_base.base.as_mut().expect("base flashblock has a base payload").parent_hash =
        B256::repeat_byte(0x42);
    let wrong_delta = FlashblockBuilder::new(&test, 1).with_canonical_block_number(1).build();
    test.flashblocks.on_flashblock_received(wrong_base);
    test.flashblocks.on_flashblock_received(wrong_delta);
    sleep(Duration::from_millis(100)).await;

    assert_eq!(
        test.flashblocks
            .get_pending_blocks()
            .as_ref()
            .expect("existing pending state remains available")
            .latest_block_number(),
        1
    );

    test.flashblocks.on_flashblock_received(
        FlashblockBuilder::new_base(&test).with_canonical_block_number(1).build(),
    );
    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks
                .get_pending_blocks()
                .as_ref()
                .is_some_and(|pending| pending.latest_block_number() == 2)
        },
        "a corrected base must extend the pending sequence",
    )
    .await;
}

#[tokio::test]
async fn test_pending_depth_remains_bounded_during_canonical_stall() {
    let test = FlashblocksBuilderTestHarness::new().await;
    for parent in 0..=5 {
        test.send_flashblock(
            FlashblockBuilder::new_base(&test).with_canonical_block_number(parent).build(),
        )
        .await;
    }

    wait_until(
        Duration::from_secs(5),
        || test.flashblocks.get_pending_blocks().is_none(),
        "pending state must clear when it exceeds the configured depth",
    )
    .await;
}

#[tokio::test]
async fn test_live_payload_flashblock_count_is_bounded() {
    let test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;
    for index in 1..=MAX_FLASHBLOCKS_PER_PAYLOAD {
        test.send_flashblock(FlashblockBuilder::new(&test, index).build()).await;
    }

    wait_until(
        Duration::from_secs(5),
        || test.flashblocks.get_pending_blocks().is_none(),
        "pending state must clear when one payload exceeds the flashblock limit",
    )
    .await;
}

#[tokio::test]
async fn test_stale_queue_clears_pending_and_resumes_at_current_tip() {
    let mut test = FlashblocksBuilderTestHarness::new().await;
    test.send_flashblock(FlashblockBuilder::new_base(&test).build()).await;

    let mut stale_flashblocks = Vec::new();
    for parent in 1..8 {
        stale_flashblocks
            .push(FlashblockBuilder::new_base(&test).with_canonical_block_number(parent).build());
    }

    let mut stale_canonicals = Vec::new();
    for _ in 0..8 {
        stale_canonicals.push(test.new_canonical_block_without_processing(vec![]).await);
    }
    let best = test.node.latest_block();

    test.flashblocks.on_canonical_block_received(stale_canonicals.remove(0));
    wait_until(
        Duration::from_secs(5),
        || test.flashblocks.get_pending_blocks().is_none(),
        "provider-tip lag must clear the stale published snapshot",
    )
    .await;

    for block in stale_canonicals {
        test.flashblocks.on_canonical_block_received(block);
    }
    for flashblock in stale_flashblocks {
        test.flashblocks.on_flashblock_received(flashblock);
    }
    sleep(Duration::from_millis(100)).await;
    assert!(test.flashblocks.get_pending_blocks().is_none());

    let resume = FlashblockBuilder::new_base(&test).build();
    assert_eq!(resume.metadata.block_number, best.number + 1);
    assert_eq!(resume.base.as_ref().map(|base| base.parent_hash), Some(best.hash()));
    test.flashblocks.on_flashblock_received(resume);

    wait_until(
        Duration::from_secs(5),
        || {
            test.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == best.number + 1
                    && pending.parent_hash() == best.hash()
            })
        },
        "processor should resume only from the current tip child",
    )
    .await;
}

#[tokio::test]
async fn test_deferred_canonical_rejects_resume_against_stale_provider_tip() {
    let client = FlashblocksBuilderTestHarness::new().await;
    let mut builder = FlashblocksBuilderTestHarness::new().await;
    let old_base = FlashblockBuilder::new_base(&client).build();
    client.send_flashblock(old_base.clone()).await;

    let canonical = builder.new_canonical_block_without_processing(vec![]).await;
    client.flashblocks.on_canonical_block_received(canonical);
    wait_until(
        Duration::from_secs(5),
        || client.flashblocks.get_pending_blocks().is_none(),
        "canonical ahead of provider visibility must clear pending state",
    )
    .await;

    client.send_flashblock(old_base).await;
    assert!(
        client.flashblocks.get_pending_blocks().is_none(),
        "deferred canonical watermark must reject resume against the stale provider tip"
    );
}

#[tokio::test]
async fn test_deferred_canonical_replays_base_received_after_notification() {
    let mut client = FlashblocksBuilderTestHarness::new().await;
    let mut builder = FlashblocksBuilderTestHarness::new().await;
    let canonical = builder.new_canonical_block_without_processing(vec![]).await;
    let canonical_hash = canonical.hash();
    let next_base = FlashblockBuilder::new_base(&builder).build();

    client.flashblocks.on_canonical_block_received(canonical);
    client.send_flashblock(next_base).await;
    let client_canonical = client.new_canonical_block_without_processing(vec![]).await;
    assert_eq!(client_canonical.hash(), canonical_hash);

    wait_until(
        Duration::from_secs(5),
        || {
            client.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 2 && pending.parent_hash() == canonical_hash
            })
        },
        "base received after an ahead canonical should replay when it becomes visible",
    )
    .await;
}

#[tokio::test]
async fn test_deferred_canonical_preserves_base_received_before_notification() {
    let mut client = FlashblocksBuilderTestHarness::new().await;
    let mut builder = FlashblocksBuilderTestHarness::new().await;
    let canonical = builder.new_canonical_block_without_processing(vec![]).await;
    let canonical_hash = canonical.hash();
    let next_base = FlashblockBuilder::new_base(&builder).build();

    client.send_flashblock(next_base).await;
    client.flashblocks.on_canonical_block_received(canonical);
    let client_canonical = client.new_canonical_block_without_processing(vec![]).await;
    assert_eq!(client_canonical.hash(), canonical_hash);

    wait_until(
        Duration::from_secs(5),
        || {
            client.flashblocks.get_pending_blocks().as_ref().is_some_and(|pending| {
                pending.earliest_block_number() == 2 && pending.parent_hash() == canonical_hash
            })
        },
        "base received before an ahead canonical should survive until provider visibility",
    )
    .await;
}
