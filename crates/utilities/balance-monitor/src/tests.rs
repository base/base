//! Tests for the balance monitor.

use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use tokio_util::sync::CancellationToken;

use crate::BalanceMonitorLayer;

#[tokio::test]
async fn publishes_balance_on_new_block() {
    // Anvil with 1-second block time so watch_blocks() fires automatically.
    let anvil = alloy_node_bindings::Anvil::new().block_time(1).spawn();
    let address: Address = anvil.addresses()[0];
    let cancel = CancellationToken::new();

    let (layer, mut balance_rx) = BalanceMonitorLayer::new(address, cancel.clone());
    let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

    // Wait for at least one balance update.
    tokio::time::timeout(std::time::Duration::from_secs(10), balance_rx.changed())
        .await
        .expect("timed out waiting for balance update")
        .expect("watch channel closed unexpectedly");

    let balance = *balance_rx.borrow();
    assert!(balance > U256::ZERO, "balance should be positive, got {balance}");

    cancel.cancel();
}

#[tokio::test]
async fn cancellation_closes_channel() {
    let anvil = alloy_node_bindings::Anvil::new().spawn();
    let address: Address = anvil.addresses()[0];
    let cancel = CancellationToken::new();

    let (layer, mut balance_rx) = BalanceMonitorLayer::new(address, cancel.clone());
    let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

    // Cancel immediately — the background task should exit and the sender
    // will be dropped, causing `changed()` to return `Err`.
    cancel.cancel();

    // The channel should close once the spawned task observes cancellation.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if balance_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "channel should close after cancellation");
}
