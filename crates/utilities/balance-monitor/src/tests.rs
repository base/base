//! Tests for the balance monitor.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use tokio_util::sync::CancellationToken;

use crate::BalanceMonitorLayer;

#[tokio::test]
async fn publishes_balance_on_first_tick() {
    let anvil = alloy_node_bindings::Anvil::new().spawn();
    let address: Address = anvil.addresses()[0];
    let cancel = CancellationToken::new();

    let (layer, mut balance_rx) =
        BalanceMonitorLayer::new(address, cancel.clone(), Duration::from_millis(100));
    let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

    tokio::time::timeout(Duration::from_secs(5), balance_rx.changed())
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

    let (layer, mut balance_rx) = BalanceMonitorLayer::new(
        address,
        cancel.clone(),
        BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
    );
    let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if balance_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "channel should close after cancellation");
}
