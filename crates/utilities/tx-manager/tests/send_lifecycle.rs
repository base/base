//! Integration tests for the transaction send lifecycle with Anvil.
//!
//! Covers [`SimpleTxManager::send`], [`SimpleTxManager::publish_tx`], and
//! [`SimpleTxManager::query_receipt`] — the methods that drive a transaction
//! from publication through confirmation.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_network::{EthereumWallet, TxSigner};
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, Signature, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_tx_manager::{
    SendState, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
};
use rstest::rstest;

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

/// Config optimized for fast test execution: single confirmation, fast
/// receipt polling, and a long resubmission timeout to prevent fee bumps
/// during the test.
fn fast_send_config() -> TxManagerConfig {
    TxManagerConfig {
        num_confirmations: 1,
        receipt_query_interval: Duration::from_millis(100),
        resubmission_timeout: Duration::from_secs(60),
        ..TxManagerConfig::default()
    }
}

/// A signer that always fails so `send()` exits before any publish.
struct FailingSigner {
    address: Address,
}

#[async_trait]
impl TxSigner<Signature> for FailingSigner {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        _tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        Err(alloy_signer::Error::other("deliberately failing signer"))
    }
}

async fn setup_with_failing_signer_config(
    config: TxManagerConfig,
) -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let address = anvil.addresses()[0];
    let wallet = EthereumWallet::from(FailingSigner { address });
    let chain_id = anvil.chain_id();
    let manager = SimpleTxManager::new(provider, wallet, config, chain_id)
        .await
        .expect("should create manager with failing signer");
    (manager, anvil)
}

async fn assert_send_error_resets_nonce(config: TxManagerConfig) {
    let (manager, _anvil) = setup_with_failing_signer_config(config).await;
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    let err = manager.send(candidate).await.expect_err("send should fail before publish");
    assert!(matches!(err, TxManagerError::Sign(_)), "expected sign error, got {err:?}");

    let guard = manager.nonce_manager().next_nonce().await.expect("should reserve nonce");
    assert_eq!(guard.nonce(), 0, "nonce manager should be reset after a pre-publish send failure",);
}

// ── send() ────────────────────────────────────────────────────────────

/// Happy-path test: send a simple value transfer through the full
/// `send_tx` event loop and verify the returned receipt.
#[tokio::test]
async fn send_confirms_simple_value_transfer() {
    let (manager, _anvil) = setup_with_config(fast_send_config()).await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let receipt = tokio::time::timeout(Duration::from_secs(10), manager.send(candidate))
        .await
        .expect("send should complete within 10 s")
        .expect("send should succeed");

    assert!(receipt.block_number.is_some(), "receipt should have a block number");
    assert_ne!(receipt.transaction_hash, B256::ZERO, "tx hash should be non-zero");
}

/// Happy-path test for `send_async`: the returned `SendHandle` resolves
/// to a valid receipt.
#[tokio::test]
async fn send_async_confirms_simple_value_transfer() {
    let (manager, _anvil) = setup_with_config(fast_send_config()).await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let handle = manager.send_async(candidate).await;

    let receipt = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("send_async should complete within 10 s")
        .expect("send_async should succeed");

    assert!(receipt.block_number.is_some(), "receipt should have a block number");
}

#[rstest]
#[case::timeout_disabled(Duration::ZERO)]
#[case::timeout_enabled(Duration::from_secs(5))]
#[tokio::test]
async fn send_resets_nonce_manager_on_pre_publish_failure(#[case] tx_send_timeout: Duration) {
    let config = TxManagerConfig { tx_send_timeout, ..fast_send_config() };
    assert_send_error_resets_nonce(config).await;
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
