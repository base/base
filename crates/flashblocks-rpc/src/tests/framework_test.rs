//! Integration tests using the new test-utils framework
//!
//! These tests demonstrate using the test-utils framework with:
//! - TestNode for node setup
//! - EngineContext for canonical block production
//! - FlashblocksContext for pending state testing
//! - Pre-funded test accounts

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use alloy_provider::Provider;
use base_reth_test_utils::{EngineContext, TestNode};
use eyre::Result;

#[tokio::test]
async fn test_framework_node_setup() -> Result<()> {
    reth_tracing::init_test_tracing();

    // Create test node with Base Sepolia and pre-funded accounts
    let node = TestNode::new().await?;
    let provider = node.provider().await?;

    // Verify chain ID
    let chain_id = provider.get_chain_id().await?;
    assert_eq!(chain_id, 84532); // Base Sepolia

    // Verify test accounts are funded
    let alice_balance = node.get_balance(node.alice().address).await?;
    assert!(alice_balance > U256::ZERO, "Alice should have initial balance");

    let bob_balance = node.get_balance(node.bob().address).await?;
    assert!(bob_balance > U256::ZERO, "Bob should have initial balance");

    Ok(())
}

#[tokio::test]
async fn test_framework_engine_api_block_production() -> Result<()> {
    reth_tracing::init_test_tracing();

    let node = TestNode::new().await?;
    let provider = node.provider().await?;

    // Get genesis block
    let genesis = provider
        .get_block_by_number(BlockNumberOrTag::Number(0))
        .await?
        .expect("Genesis block should exist");

    let genesis_hash = genesis.header.hash;

    // Create engine context for canonical block production
    let mut engine = EngineContext::new(
        node.http_url(),
        genesis_hash,
        genesis.header.timestamp,
    )
    .await?;

    // Build and finalize a single canonical block
    let block_1_hash = engine.build_and_finalize_block().await?;
    assert_ne!(block_1_hash, genesis_hash);
    assert_eq!(engine.block_number(), 1);

    // Verify the block exists
    let block_1 = provider
        .get_block_by_hash(block_1_hash)
        .await?
        .expect("Block 1 should exist");
    assert_eq!(block_1.header.number, 1);

    // Advance chain by multiple blocks
    let block_hashes = engine.advance_chain(3).await?;
    assert_eq!(block_hashes.len(), 3);
    assert_eq!(engine.block_number(), 4);

    // Verify latest block
    let latest = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .expect("Latest block should exist");
    assert_eq!(latest.header.number, 4);
    assert_eq!(latest.header.hash, engine.head_hash());

    Ok(())
}

#[tokio::test]
async fn test_framework_account_balances() -> Result<()> {
    reth_tracing::init_test_tracing();

    let node = TestNode::new().await?;
    let provider = node.provider().await?;

    // Check all test accounts have their initial balances
    let accounts = &node.accounts;

    for account in accounts.all() {
        let balance = provider.get_balance(account.address).await?;
        assert_eq!(
            balance,
            account.initial_balance_wei(),
            "{} should have initial balance",
            account.name
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_framework_parallel_nodes() -> Result<()> {
    reth_tracing::init_test_tracing();

    // Launch multiple nodes in parallel to verify isolation
    let (node1_result, node2_result) = tokio::join!(TestNode::new(), TestNode::new());

    let node1 = node1_result?;
    let node2 = node2_result?;

    // Verify they have different ports
    assert_ne!(node1.http_api_addr, node2.http_api_addr);

    // Verify both are functional
    let provider1 = node1.provider().await?;
    let provider2 = node2.provider().await?;

    let chain_id_1 = provider1.get_chain_id().await?;
    let chain_id_2 = provider2.get_chain_id().await?;

    assert_eq!(chain_id_1, 84532);
    assert_eq!(chain_id_2, 84532);

    Ok(())
}
