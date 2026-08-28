//! End-to-end lifecycle tests against Anvil.

pub mod common;

use std::time::Duration;

use alloy_provider::Provider;
use base_tx_manager::{SubmissionStatus, TxManager, TxManagerConfig};
use common::{mine_block, setup_with_config, value_transfer};

fn fast_config() -> TxManagerConfig {
    TxManagerConfig { resubmission_timeout: Duration::from_secs(1), ..common::fast_config() }
}

#[tokio::test]
async fn send_publishes_and_resolves_a_transaction() {
    let (manager, _provider, _anvil) = setup_with_config(fast_config()).await;

    let receipt =
        manager.submit(value_transfer(1_000)).wait().await.expect("transaction should confirm");

    assert_eq!(receipt.from, manager.sender_address());
    assert!(receipt.status());
}

#[tokio::test]
async fn concurrent_submissions_receive_sequential_nonces() {
    let (manager, _provider, _anvil) = setup_with_config(fast_config()).await;
    let first = manager.submit(value_transfer(1));
    let second = manager.submit(value_transfer(2));

    let (first, second) = tokio::join!(first.wait(), second.wait());
    let first = first.expect("first transaction should confirm");
    let second = second.expect("second transaction should confirm");

    assert_ne!(first.transaction_hash, second.transaction_hash);
    assert_eq!(first.from, second.from);
}

#[tokio::test]
async fn caller_can_stop_waiting_without_abandoning_a_committed_nonce() {
    let (manager, provider, _anvil) = setup_with_config(fast_config()).await;
    provider
        .raw_request::<_, Option<bool>>("evm_setAutomine".into(), [false])
        .await
        .expect("automine should be disabled");

    let send = manager.submit(value_transfer(1));
    let status = send.clone();
    assert!(tokio::time::timeout(Duration::from_millis(50), send.wait()).await.is_err());
    assert!(matches!(status.snapshot().status, SubmissionStatus::Pending { .. }));

    mine_block(&provider).await;
    let receipt = tokio::time::timeout(Duration::from_secs(2), status.wait())
        .await
        .expect("status should eventually resolve")
        .expect("mined transaction should confirm");
    assert!(receipt.status());
}
