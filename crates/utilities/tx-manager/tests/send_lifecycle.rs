//! Integration tests for [`SimpleTxManager::publish_tx`] with Anvil.

use std::time::Duration;

use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{SendState, SimpleTxManager, TxCandidate, TxManagerConfig};

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
