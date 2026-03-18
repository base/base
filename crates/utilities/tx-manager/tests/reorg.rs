//! Integration tests for L1 reorg edge cases.
//!
//! Uses Anvil's `evm_snapshot` / `evm_revert` RPCs to simulate chain
//! reorganizations and verifies that [`SimpleTxManager::query_receipt`],
//! [`SendState`], and [`NonceManager`] behave correctly when blocks are
//! reorged out.

use std::{sync::Arc, time::Duration};

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{NoopTxMetrics, SendState, SimpleTxManager, TxCandidate, TxManagerConfig};

// ── Helpers ────────────────────────────────────────────────────────────

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

async fn manager_from_anvil(
    anvil: &alloy_node_bindings::AnvilInstance,
    config: TxManagerConfig,
) -> SimpleTxManager {
    let provider = RootProvider::new_http(anvil.endpoint_url());
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    SimpleTxManager::from_wallet(
        provider,
        wallet,
        config,
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect("should create manager")
}

async fn setup_with_config(
    config: TxManagerConfig,
) -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = alloy_node_bindings::Anvil::new().spawn();
    let manager = manager_from_anvil(&anvil, config).await;
    (manager, anvil)
}

async fn mine_block(provider: &RootProvider) {
    provider
        .raw_request::<(), String>("evm_mine".into(), ())
        .await
        .expect("evm_mine should succeed");
}

async fn mine_blocks(provider: &RootProvider, n: usize) {
    for _ in 0..n {
        mine_block(provider).await;
    }
}

async fn snapshot(provider: &RootProvider) -> U256 {
    provider
        .raw_request::<(), U256>("evm_snapshot".into(), ())
        .await
        .expect("evm_snapshot should succeed")
}

async fn revert(provider: &RootProvider, id: U256) {
    let success: bool = provider
        .raw_request::<[U256; 1], bool>("evm_revert".into(), [id])
        .await
        .expect("evm_revert should succeed");
    assert!(success, "evm_revert should return true");
}

async fn publish_simple_tx(manager: &SimpleTxManager) -> (B256, SendState) {
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };
    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let send_state = SendState::new(3).expect("should create send state");
    let tx_hash =
        manager.publish_tx(&send_state, &prepared.raw_tx, None).await.expect("should publish tx");
    (tx_hash, send_state)
}

async fn query(
    send_state: &SendState,
    manager: &SimpleTxManager,
    tx_hash: B256,
    confirmations: u64,
) -> Option<alloy_rpc_types_eth::TransactionReceipt> {
    SimpleTxManager::query_receipt(
        send_state,
        manager.provider(),
        tx_hash,
        confirmations,
        QUERY_TIMEOUT,
    )
    .await
    .expect("query should not error")
}

// ── Tests ──────────────────────────────────────────────────────────────

/// Receipt disappears after a reorg removes the tx's block.
#[tokio::test]
async fn query_receipt_returns_none_after_reorg_removes_tx() {
    let config = TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() };
    let (manager, _anvil) = setup_with_config(config).await;

    // Snapshot before sending tx.
    let snap = snapshot(manager.provider()).await;

    // Send tx, auto-mine includes it.
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;

    // Mine extra block for confirmation depth.
    mine_block(manager.provider()).await;

    let receipt = query(&send_state, &manager, tx_hash, 1).await;
    assert!(receipt.is_some(), "receipt should exist before reorg");
    assert!(send_state.is_waiting_for_confirmation());

    // Revert to pre-tx snapshot (simulates reorg removing tx's block).
    revert(manager.provider(), snap).await;

    // Mine a new empty block on the reorged chain.
    mine_block(manager.provider()).await;

    let receipt = query(&send_state, &manager, tx_hash, 1).await;
    assert!(receipt.is_none(), "receipt should be gone after reorg");
    assert!(
        !send_state.is_waiting_for_confirmation(),
        "tx should no longer be tracked as mined after reorg",
    );
}

/// Tx is reorged into a different block. The receipt is accepted because the
/// re-included tx's block hash matches the canonical chain after the reorg.
#[tokio::test]
async fn query_receipt_returns_receipt_after_reorg_reinclusion() {
    let config = TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() };
    let anvil = alloy_node_bindings::Anvil::new().arg("--no-mining").spawn();
    let manager = manager_from_anvil(&anvil, config).await;

    // Craft and publish tx (sits in mempool since automine is off).
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };
    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let send_state = SendState::new(3).expect("should create send state");
    let tx_hash =
        manager.publish_tx(&send_state, &prepared.raw_tx, None).await.expect("should publish tx");

    // Snapshot while tx is in mempool.
    let snap = snapshot(manager.provider()).await;

    // Mine block — tx gets included.
    mine_block(manager.provider()).await;

    let receipt = manager
        .provider()
        .get_transaction_receipt(tx_hash)
        .await
        .expect("should fetch receipt")
        .expect("receipt should exist");
    let original_block_hash = receipt.block_hash.expect("should have block hash");

    // Revert to snapshot — chain state back to pre-mine.
    revert(manager.provider(), snap).await;

    // Re-submit the same raw tx (Anvil may not restore the mempool on revert).
    let _ = manager
        .provider()
        .send_raw_transaction(&prepared.raw_tx)
        .await
        .expect("re-submit should succeed");

    // Mine block again — tx re-included in a new block with different hash.
    mine_block(manager.provider()).await;

    // Mine confirmation block.
    mine_block(manager.provider()).await;

    let new_receipt = manager
        .provider()
        .get_transaction_receipt(tx_hash)
        .await
        .expect("should fetch receipt")
        .expect("receipt should exist after reinclusion");
    let new_block_hash = new_receipt.block_hash.expect("should have block hash");

    assert_ne!(
        original_block_hash, new_block_hash,
        "block hash should change after reorg reinclusion",
    );

    // query_receipt validates the receipt's block hash against the canonical
    // chain. The re-included tx is on the canonical chain, so it should pass.
    let result = query(&send_state, &manager, tx_hash, 1).await;
    assert!(result.is_some(), "receipt should exist after reinclusion");
}

/// Nonce conflicts from reorged txs — freed nonce handled correctly
/// after reset.
#[tokio::test]
async fn nonce_manager_recovers_correct_nonce_after_reorg() {
    let config = TxManagerConfig::default();
    let (manager, anvil) = setup_with_config(config).await;
    let address = anvil.addresses()[0];

    // Snapshot before sending tx.
    let snap = snapshot(manager.provider()).await;

    // Send tx (consumes nonce 0), auto-mine includes it.
    publish_simple_tx(&manager).await;
    mine_block(manager.provider()).await;

    let chain_nonce =
        manager.provider().get_transaction_count(address).await.expect("should get tx count");
    assert_eq!(chain_nonce, 1, "chain nonce should be 1 after tx");

    // Revert to pre-tx snapshot.
    revert(manager.provider(), snap).await;
    mine_block(manager.provider()).await;

    let chain_nonce =
        manager.provider().get_transaction_count(address).await.expect("should get tx count");
    assert_eq!(chain_nonce, 0, "chain nonce should be 0 after reorg");

    // Reset nonce manager to clear stale cache.
    manager.nonce_manager().reset().await;

    // next_nonce should return 0 (correct chain nonce after reset).
    let guard = manager.nonce_manager().next_nonce().await.expect("should get nonce");
    assert_eq!(guard.nonce(), 0, "nonce should match chain state after reorg and reset");
}

/// High-water mark prevents reissue of reserved nonces even after
/// reorg + reset.
#[tokio::test]
async fn reserved_nonce_high_water_mark_survives_reorg() {
    let config = TxManagerConfig::default();
    let (manager, _anvil) = setup_with_config(config).await;

    // Snapshot before reserving nonces.
    let snap = snapshot(manager.provider()).await;

    // Reserve nonces 0 and 1.
    let n0 = manager.nonce_manager().reserve_nonce().await.expect("should reserve nonce 0");
    let n1 = manager.nonce_manager().reserve_nonce().await.expect("should reserve nonce 1");
    assert_eq!(n0, 0);
    assert_eq!(n1, 1);

    // Revert and mine an empty block.
    revert(manager.provider(), snap).await;
    mine_block(manager.provider()).await;

    // Reset nonce manager to clear stale cache.
    manager.nonce_manager().reset().await;

    // High-water mark (2) prevents reissue of nonces 0 and 1.
    let guard = manager.nonce_manager().next_nonce().await.expect("should get nonce");
    assert_eq!(guard.nonce(), 2, "high-water mark should prevent reissue of reserved nonces");
}

/// Even a fully confirmed tx disappears under a deep reorg past
/// the confirmation depth.
#[tokio::test]
async fn deep_reorg_past_confirmation_depth_removes_receipt() {
    let config = TxManagerConfig { num_confirmations: 3, ..TxManagerConfig::default() };
    let (manager, _anvil) = setup_with_config(config).await;

    // Snapshot before sending tx.
    let snap = snapshot(manager.provider()).await;

    // Send tx, auto-mine includes it.
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;

    // Mine 2 extra blocks for full confirmation (tx_block + 3 <= tip + 1).
    mine_blocks(manager.provider(), 2).await;

    let receipt = query(&send_state, &manager, tx_hash, 3).await;
    assert!(receipt.is_some(), "receipt should be fully confirmed before reorg");

    // Revert to pre-tx snapshot (deep reorg past confirmation depth).
    revert(manager.provider(), snap).await;

    // Mine new empty blocks.
    mine_blocks(manager.provider(), 3).await;

    let receipt = query(&send_state, &manager, tx_hash, 3).await;
    assert!(receipt.is_none(), "receipt should be gone after deep reorg");
    assert!(
        !send_state.is_waiting_for_confirmation(),
        "tx should no longer be tracked as mined after deep reorg",
    );
}

/// Shallow reorg doesn't affect a deeply confirmed tx.
#[tokio::test]
async fn shallow_reorg_preserves_deeply_confirmed_tx() {
    let config = TxManagerConfig { num_confirmations: 3, ..TxManagerConfig::default() };
    let (manager, _anvil) = setup_with_config(config).await;

    // Send tx, auto-mine includes it.
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;

    // Mine 3 more blocks (tx is 4 blocks deep).
    mine_blocks(manager.provider(), 3).await;

    // Snapshot at this point.
    let snap = snapshot(manager.provider()).await;

    // Mine 2 more blocks.
    mine_blocks(manager.provider(), 2).await;

    // Revert to snapshot (removes last 2 blocks, but tx's block survives).
    revert(manager.provider(), snap).await;

    // Mine 2 replacement empty blocks.
    mine_blocks(manager.provider(), 2).await;

    let receipt = query(&send_state, &manager, tx_hash, 3).await;
    assert!(receipt.is_some(), "deeply confirmed tx should survive shallow reorg");
}
