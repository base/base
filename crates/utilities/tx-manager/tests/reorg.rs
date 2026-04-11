//! Integration tests for L1 reorg edge cases.
//!
//! Uses Anvil's `evm_snapshot` / `evm_revert` RPCs to simulate chain
//! reorganizations and verifies that [`SimpleTxManager::query_receipt`],
//! [`SendState`], and [`NonceManager`] behave correctly when blocks are
//! reorged out.

mod common;

use std::{sync::Arc, time::Duration};

use alloy_network::EthereumWallet;
use alloy_primitives::{B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{NoopTxMetrics, SendState, SimpleTxManager, TxManagerConfig};
use common::{mine_block, publish_simple_tx, setup_with_config};

// ── Helpers ────────────────────────────────────────────────────────────

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Creates a [`SimpleTxManager`] from an existing Anvil instance, needed
/// for tests that customize Anvil flags (e.g. `--no-mining`).
async fn manager_from_anvil(
    anvil: &alloy_node_bindings::AnvilInstance,
    config: TxManagerConfig,
) -> SimpleTxManager<RootProvider> {
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

async fn query(
    send_state: &SendState,
    manager: &SimpleTxManager<RootProvider>,
    tx_hash: B256,
) -> Option<alloy_rpc_types_eth::TransactionReceipt> {
    SimpleTxManager::query_receipt(
        send_state,
        manager.provider(),
        tx_hash,
        manager.config().num_confirmations,
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

    let snap = snapshot(manager.provider()).await;
    let (tx_hash, send_state, _) = publish_simple_tx(&manager).await;
    mine_block(manager.provider()).await;

    let receipt = query(&send_state, &manager, tx_hash).await;
    assert!(receipt.is_some(), "receipt should exist before reorg");
    assert!(send_state.is_waiting_for_confirmation());

    revert(manager.provider(), snap).await;
    mine_block(manager.provider()).await;

    let receipt = query(&send_state, &manager, tx_hash).await;
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

    // Tx sits in mempool since automine is off.
    let (tx_hash, send_state, raw_tx) = publish_simple_tx(&manager).await;

    let snap = snapshot(manager.provider()).await;
    mine_block(manager.provider()).await;

    let receipt = manager
        .provider()
        .get_transaction_receipt(tx_hash)
        .await
        .expect("should fetch receipt")
        .expect("receipt should exist");
    let original_block_hash = receipt.block_hash.expect("should have block hash");

    revert(manager.provider(), snap).await;

    // Anvil may not restore the mempool on revert, so re-submit.
    let _ =
        manager.provider().send_raw_transaction(&raw_tx).await.expect("re-submit should succeed");

    mine_block(manager.provider()).await;
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
    let result = query(&send_state, &manager, tx_hash).await;
    assert!(result.is_some(), "receipt should exist after reinclusion");
}

/// Nonce conflicts from reorged txs — freed nonce handled correctly
/// after reset.
#[tokio::test]
async fn nonce_manager_recovers_correct_nonce_after_reorg() {
    let config = TxManagerConfig::default();
    let (manager, anvil) = setup_with_config(config).await;
    let address = anvil.addresses()[0];

    let snap = snapshot(manager.provider()).await;

    // Consumes nonce 0.
    let _ = publish_simple_tx(&manager).await;
    mine_block(manager.provider()).await;

    let chain_nonce =
        manager.provider().get_transaction_count(address).await.expect("should get tx count");
    assert_eq!(chain_nonce, 1, "chain nonce should be 1 after tx");

    revert(manager.provider(), snap).await;
    mine_block(manager.provider()).await;

    let chain_nonce =
        manager.provider().get_transaction_count(address).await.expect("should get tx count");
    assert_eq!(chain_nonce, 0, "chain nonce should be 0 after reorg");

    manager.nonce_manager().reset().await;

    let guard = manager.nonce_manager().next_nonce().await.expect("should get nonce");
    assert_eq!(guard.nonce(), 0, "nonce should match chain state after reorg and reset");
}

/// High-water mark prevents reissue of reserved nonces even after
/// reorg + reset.
#[tokio::test]
async fn reserved_nonce_high_water_mark_survives_reorg() {
    let config = TxManagerConfig::default();
    let (manager, _anvil) = setup_with_config(config).await;

    let snap = snapshot(manager.provider()).await;

    let n0 = manager.nonce_manager().reserve_nonce().await.expect("should reserve nonce 0");
    let n1 = manager.nonce_manager().reserve_nonce().await.expect("should reserve nonce 1");
    assert_eq!(n0, 0);
    assert_eq!(n1, 1);

    revert(manager.provider(), snap).await;
    mine_block(manager.provider()).await;

    manager.nonce_manager().reset().await;

    let guard = manager.nonce_manager().next_nonce().await.expect("should get nonce");
    assert_eq!(guard.nonce(), 2, "high-water mark should prevent reissue of reserved nonces");
}

/// Even a fully confirmed tx disappears under a deep reorg past
/// the confirmation depth.
#[tokio::test]
async fn deep_reorg_past_confirmation_depth_removes_receipt() {
    let config = TxManagerConfig { num_confirmations: 3, ..TxManagerConfig::default() };
    let (manager, _anvil) = setup_with_config(config).await;

    let snap = snapshot(manager.provider()).await;

    let (tx_hash, send_state, _) = publish_simple_tx(&manager).await;
    // 4 blocks: 1 to flush auto-mine (may be deferred under CI load) + 3 confirmations.
    mine_blocks(manager.provider(), 4).await;

    let receipt = query(&send_state, &manager, tx_hash).await;
    assert!(receipt.is_some(), "receipt should be fully confirmed before reorg");

    revert(manager.provider(), snap).await;
    mine_blocks(manager.provider(), 3).await;

    let receipt = query(&send_state, &manager, tx_hash).await;
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

    let (tx_hash, send_state, _) = publish_simple_tx(&manager).await;

    // Tx is 4 blocks deep after these mines.
    mine_blocks(manager.provider(), 3).await;

    // Snapshot here; the revert only removes the 2 blocks mined after it.
    let snap = snapshot(manager.provider()).await;
    mine_blocks(manager.provider(), 2).await;

    revert(manager.provider(), snap).await;
    mine_blocks(manager.provider(), 2).await;

    let receipt = query(&send_state, &manager, tx_hash).await;
    assert!(receipt.is_some(), "deeply confirmed tx should survive shallow reorg");
}
