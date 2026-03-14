//! Integration tests for the transaction send lifecycle with Anvil.
//!
//! Covers [`SimpleTxManager::send`], [`SimpleTxManager::publish_tx`], and
//! [`SimpleTxManager::query_receipt`] — the methods that drive a transaction
//! from publication through confirmation.

use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use alloy_consensus::{SignableTransaction, Transaction};
use alloy_network::{EthereumWallet, TxSigner};
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, Signature, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_tx_manager::{
    SendState, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
};
use rstest::rstest;
use tokio::sync::mpsc;

/// Force-mine a block on Anvil so receipts are committed before queries.
///
/// Under CI load Anvil's auto-mine may not have flushed yet, causing
/// `query_receipt` to see stale tip heights or missing receipts.
async fn mine_block(manager: &SimpleTxManager) {
    manager
        .provider()
        .raw_request::<(), String>("evm_mine".into(), ())
        .await
        .expect("evm_mine should succeed");
}

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

    // Mine an extra block so tip_height is strictly ahead of the tx block.
    // query_receipt fetches tip_height before the receipt (for reorg safety),
    // so under CI load the tip may be stale on a single-call test.
    mine_block(&manager).await;

    // With num_confirmations = 1 and the extra block mined above,
    // the formula tx_block + 1 <= tip + 1 is satisfied.
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

    // Mine an extra block so the receipt is available before we query.
    // Under CI load Anvil's auto-mine may not have committed yet.
    mine_block(&manager).await;

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
    let config = TxManagerConfig {
        num_confirmations: 100,
        receipt_query_interval: Duration::from_millis(50),
        ..TxManagerConfig::default()
    };
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

    let _handle = SimpleTxManager::wait_for_tx(
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
    let closed = Arc::new(AtomicBool::new(true));
    let (receipt_tx, mut receipt_rx) = mpsc::channel(1);

    let _handle = SimpleTxManager::wait_for_tx(
        Arc::new(send_state),
        manager.provider().clone(),
        tx_hash,
        config,
        receipt_tx,
        closed,
    );

    let result = tokio::time::timeout(Duration::from_secs(5), receipt_rx.recv())
        .await
        .expect("should not time out");
    assert!(result.is_none(), "channel should close without delivering a receipt");
}

/// `wait_for_tx` exits cleanly without panicking when the receiver is
/// dropped before the task delivers a receipt.
#[tokio::test]
async fn wait_for_tx_does_not_panic_on_dropped_receiver() {
    let config = fast_polling_config();
    let (manager, _anvil) = setup_with_config(config.clone()).await;
    let (tx_hash, send_state) = publish_simple_tx(&manager).await;
    let closed = Arc::new(AtomicBool::new(false));
    let (receipt_tx, receipt_rx) = mpsc::channel(1);

    let handle = SimpleTxManager::wait_for_tx(
        Arc::new(send_state),
        manager.provider().clone(),
        tx_hash,
        config,
        receipt_tx,
        closed,
    );

    // Drop receiver immediately — task should exit cleanly.
    drop(receipt_rx);

    // Await the handle to confirm the task exits without panic.
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("task should not time out")
        .expect("task should not panic");
}

// ── query_receipt() error paths ───────────────────────────────────────

/// `query_receipt` returns `Err` when the provider is unreachable.
#[tokio::test]
async fn query_receipt_returns_error_on_unreachable_provider() {
    // Spawn Anvil to get a valid endpoint, then drop it to close the port.
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    drop(anvil);

    let send_state = SendState::new(3).expect("should create send state");
    let fake_hash = B256::with_last_byte(0xFF);

    let result = SimpleTxManager::query_receipt(
        &send_state,
        &provider,
        fake_hash,
        1,
        Duration::from_secs(2),
    )
    .await;

    assert!(result.is_err(), "query_receipt should fail when provider is unreachable");
}

// ── send_async() nonce ordering ───────────────────────────────────────

/// Sequential `send_async()` calls receive monotonically increasing nonces,
/// regardless of tokio task scheduling order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_async_preserves_call_order_nonces() {
    let (manager, _anvil) = setup_with_config(fast_send_config()).await;

    let count = 5;
    let mut handles = Vec::with_capacity(count);

    // Call send_async sequentially — each call pre-reserves a nonce before
    // spawning, so the nonce assignment matches call order.
    for _ in 0..count {
        let candidate = TxCandidate {
            to: Some(Address::with_last_byte(0x42)),
            value: U256::from(1u64),
            gas_limit: 0,
            ..Default::default()
        };
        handles.push(manager.send_async(candidate).await);
    }

    // Collect receipts (order may differ from send order).
    let mut nonces: Vec<u64> = Vec::with_capacity(count);
    for handle in handles {
        let receipt = tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("send_async should complete within 30 s")
            .expect("send_async should succeed");

        let tx = manager
            .provider()
            .get_transaction_by_hash(receipt.transaction_hash)
            .await
            .expect("should fetch tx")
            .expect("tx should exist");
        nonces.push(tx.inner.nonce());
    }

    // Nonces should form a contiguous, monotonically increasing sequence
    // starting from 0.
    let expected: Vec<u64> = (0..count as u64).collect();
    assert_eq!(nonces, expected, "send_async nonces should match call order");
}

/// When `send_async` fails before publishing (e.g., signing failure), the
/// pre-reserved nonce is returned for reuse by subsequent calls.
#[tokio::test]
async fn send_async_failure_returns_nonce_for_reuse() {
    let (manager, _anvil) = setup_with_failing_signer_config(fast_send_config()).await;
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    // send_async reserves nonce 0, spawned task fails at signing.
    let handle = manager.send_async(candidate).await;
    let err = tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("send_async should complete")
        .expect_err("send_async should fail with signing error");

    assert!(matches!(err, TxManagerError::Sign(_)), "expected sign error, got {err:?}");

    // The nonce should have been returned. Next nonce should be 0, not 1.
    let guard = manager.nonce_manager().next_nonce().await.expect("should get nonce");
    assert_eq!(guard.nonce(), 0, "returned nonce should be reused after send_async failure");
}
