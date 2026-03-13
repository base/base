//! Integration tests for [`SimpleTxManager`] send lifecycle with Anvil.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{SendState, SimpleTxManager, TxCandidate, TxManagerConfig};
use tokio::sync::mpsc;

/// Returns a config with fast polling suitable for tests.
fn fast_polling_config() -> TxManagerConfig {
    TxManagerConfig {
        num_confirmations: 1,
        receipt_query_interval: Duration::from_millis(50),
        ..TxManagerConfig::default()
    }
}

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`]
/// configured with the given [`TxManagerConfig`].
async fn setup_with_config(
    config: TxManagerConfig,
) -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let chain_id = anvil.chain_id();
    let manager = SimpleTxManager::new(provider, wallet, config, chain_id)
        .await
        .expect("should create manager");
    (manager, anvil)
}

// ── publish_tx() ──────────────────────────────────────────────────────

/// `publish_tx` broadcasts a valid raw transaction, returns a non-zero
/// hash, and records a successful publish on the `SendState`.
#[tokio::test]
async fn publish_tx_success_returns_hash_and_records_publish() {
    let (manager, _anvil) = setup_with_config(TxManagerConfig::default()).await;

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

    assert_ne!(tx_hash, B256::ZERO, "published tx hash should be non-zero");
    assert_eq!(send_state.successful_publish_count(), 1, "should record one successful publish");
}

// ── query_receipt() ───────────────────────────────────────────────────

/// `query_receipt` returns `None` for a transaction hash that does not
/// exist on chain, and records `tx_not_mined` on the `SendState`.
#[tokio::test]
async fn query_receipt_returns_none_for_unknown_tx() {
    let (manager, _anvil) = setup_with_config(TxManagerConfig::default()).await;

    let send_state = SendState::new(3).expect("should create send state");
    let fake_hash = B256::with_last_byte(0xFF);

    let result = SimpleTxManager::query_receipt(
        &send_state,
        manager.provider(),
        fake_hash,
        1,
        Duration::from_secs(10),
    )
    .await
    .expect("query should not error");

    assert!(result.is_none(), "should return None for unknown tx hash");
    // The unknown hash was never mined, so is_waiting_for_confirmation
    // should remain false (tx_not_mined only resets the counter for
    // hashes that were previously tracked).
    assert!(!send_state.is_waiting_for_confirmation(), "no tx should be tracked as mined",);
}

/// `query_receipt` returns `Some(receipt)` when the transaction is mined
/// and has reached the required confirmation depth.
#[tokio::test]
async fn query_receipt_returns_confirmed_receipt() {
    let (manager, _anvil) = setup_with_config(TxManagerConfig::default()).await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    // Craft, publish, and let Anvil auto-mine.
    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let send_state = SendState::new(3).expect("should create send state");
    let tx_hash =
        manager.publish_tx(&send_state, &prepared.raw_tx, None).await.expect("should publish tx");

    // With num_confirmations = 1 and Anvil auto-mining at the same block,
    // the formula tx_block + 1 <= tip + 1 is satisfied immediately.
    let result = SimpleTxManager::query_receipt(
        &send_state,
        manager.provider(),
        tx_hash,
        1,
        Duration::from_secs(10),
    )
    .await
    .expect("query should not error");

    let receipt = result.expect("should return a receipt for a mined tx");
    assert_eq!(receipt.transaction_hash, tx_hash, "receipt hash should match");
    assert!(receipt.block_number.is_some(), "receipt should have a block number");
    // tx_mined should have been called.
    assert!(send_state.is_waiting_for_confirmation(), "tx should be tracked as mined",);
}

/// `query_receipt` returns `None` when the receipt exists but the
/// transaction has not reached the required confirmation depth. The
/// `SendState` should still record the transaction as mined.
#[tokio::test]
async fn query_receipt_returns_none_when_not_enough_confirmations() {
    let (manager, _anvil) = setup_with_config(TxManagerConfig::default()).await;

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

    // Require 100 confirmations — far more than the single block Anvil
    // has mined. The receipt exists but is not sufficiently confirmed.
    let result = SimpleTxManager::query_receipt(
        &send_state,
        manager.provider(),
        tx_hash,
        100,
        Duration::from_secs(10),
    )
    .await
    .expect("query should not error");

    assert!(result.is_none(), "should return None when not enough confirmations");
    // The tx should still be tracked as mined even though not yet confirmed.
    assert!(
        send_state.is_waiting_for_confirmation(),
        "tx should be tracked as mined despite insufficient confirmations",
    );
}

// ── wait_mined() ──────────────────────────────────────────────────────

/// Helper: publishes a simple value-transfer tx and returns the hash.
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

/// `wait_mined` returns a confirmed receipt for a mined transaction.
#[tokio::test]
async fn wait_mined_returns_confirmed_receipt() {
    let config = fast_polling_config();
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = AtomicBool::new(false);

    let receipt =
        SimpleTxManager::wait_mined(&send_state, manager.provider(), tx_hash, &config, &closed)
            .await;

    let receipt = receipt.expect("should return a receipt");
    assert_eq!(receipt.transaction_hash, tx_hash, "receipt hash should match");
}

/// `wait_mined` returns `None` when the manager is shut down.
#[tokio::test]
async fn wait_mined_returns_none_on_shutdown() {
    let config = fast_polling_config();
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = AtomicBool::new(true);

    let receipt =
        SimpleTxManager::wait_mined(&send_state, manager.provider(), tx_hash, &config, &closed)
            .await;

    assert!(receipt.is_none(), "should return None when closed");
}

/// `wait_mined` returns `None` when the confirmation timeout expires.
#[tokio::test]
async fn wait_mined_returns_none_on_timeout() {
    let config = TxManagerConfig {
        num_confirmations: 100,
        receipt_query_interval: Duration::from_millis(50),
        confirmation_timeout: Duration::from_millis(200),
        ..TxManagerConfig::default()
    };
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = AtomicBool::new(false);

    let receipt =
        SimpleTxManager::wait_mined(&send_state, manager.provider(), tx_hash, &config, &closed)
            .await;

    assert!(receipt.is_none(), "should return None when timeout exceeded");
}

// ── wait_for_tx() ─────────────────────────────────────────────────────

/// `wait_for_tx` delivers a receipt through the mpsc channel.
#[tokio::test]
async fn wait_for_tx_delivers_receipt() {
    let config = fast_polling_config();
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = Arc::new(AtomicBool::new(false));
    let (receipt_tx, mut receipt_rx) = mpsc::channel(1);

    SimpleTxManager::wait_for_tx(
        Arc::new(send_state),
        manager.provider().clone(),
        tx_hash,
        config,
        receipt_tx,
        closed,
    );

    let receipt = tokio::time::timeout(Duration::from_secs(5), receipt_rx.recv())
        .await
        .expect("should not time out")
        .expect("channel should deliver a receipt");
    assert_eq!(receipt.transaction_hash, tx_hash, "receipt hash should match");
}

/// `wait_for_tx` closes the channel when the manager shuts down.
#[tokio::test]
async fn wait_for_tx_closes_channel_on_shutdown() {
    let config = TxManagerConfig {
        num_confirmations: 100,
        receipt_query_interval: Duration::from_millis(50),
        ..TxManagerConfig::default()
    };
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = Arc::new(AtomicBool::new(false));
    let (receipt_tx, mut receipt_rx) = mpsc::channel(1);

    SimpleTxManager::wait_for_tx(
        Arc::new(send_state),
        manager.provider().clone(),
        tx_hash,
        config,
        receipt_tx,
        Arc::clone(&closed),
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    closed.store(true, Ordering::Release);

    let result = tokio::time::timeout(Duration::from_secs(5), receipt_rx.recv())
        .await
        .expect("should not time out");
    assert!(result.is_none(), "channel should close without delivering a receipt");
}
