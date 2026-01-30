//! Integration tests that stress Flashblocks state handling.

use alloy_primitives::U256;
use base_client_node::test_utils::Account;
use base_flashblocks::{FlashblocksAPI, PendingBlocksAPI};
use base_flashblocks_node::test_harness::{FlashblockBuilder, FlashblocksBuilderTestHarness};
use op_alloy_network::BlockResponse;
use reth_provider::{AccountReader, StateProviderFactory};

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

    let pending_nonce =
        test.provider.basic_account(&Account::Alice.address()).unwrap().unwrap().nonce
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

    let pending_nonce =
        test.provider.basic_account(&Account::Alice.address()).unwrap().unwrap().nonce
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
async fn test_flashblock_for_new_canonical_block_clears_older_flashblocks_if_non_zero_index() {
    let test = FlashblocksBuilderTestHarness::new().await;

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
    use op_alloy_rpc_types::OpTransactionRequest;

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
    assert_eq!(block_one.transaction_count(), 2);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());

    test.new_canonical_block(vec![
        test.build_transaction_to_send_eth(Account::Bob, Account::Charlie, 100),
        test.build_transaction_to_send_eth(Account::Charlie, Account::Alice, 1000),
    ])
    .await;

    let block_two = test.node.latest_block();
    assert_eq!(block_two.number, 2);
    assert_eq!(block_two.transaction_count(), 3);
    assert!(test.flashblocks.get_pending_blocks().get_block(true).is_none());
}
