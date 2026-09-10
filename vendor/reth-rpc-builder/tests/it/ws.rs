#![allow(unreachable_pub)]
//! `WebSocket` subscription tests for `eth_subscribe` / `eth_unsubscribe`

use std::time::Duration;

use base_common_runtime_tasks::EventSender;
use jsonrpsee::core::client::{Subscription, SubscriptionClientT};
use reth_primitives_traits::SignedTransaction;
use reth_rpc_builder::{RpcServerConfig, TransportRpcModuleConfig};
use serde_json::Value;

use crate::utils::{launch_ws, test_rpc_registry};

/// Helper to launch a WS server with the Eth module.
async fn launch_ws_eth() -> reth_rpc_builder::RpcServerHandle {
    launch_ws().await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_all_supported_kinds_accept() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws_eth().await;
    let client = handle.ws_client().await.unwrap();

    let cases: Vec<(&str, Vec<Value>)> = vec![
        ("newHeads", vec![]),
        ("newPendingTransactions", vec![]),
        ("newPendingTransactions", vec![serde_json::json!(true)]),
        ("logs", vec![serde_json::json!({})]),
        (
            "logs",
            vec![serde_json::json!({"address": "0x0000000000000000000000000000000000000001"})],
        ),
        (
            "logs",
            vec![
                serde_json::json!({"topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"]}),
            ],
        ),
        ("transactionReceipts", vec![]),
        ("transactionReceipts", vec![serde_json::json!({"transactionHashes": []})]),
        (
            "transactionReceipts",
            vec![
                serde_json::json!({"transactionHashes": ["0x5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060"]}),
            ],
        ),
    ];

    for (kind, params) in cases {
        let mut rpc_params = jsonrpsee::core::params::ArrayParams::new();
        rpc_params.insert(kind).unwrap();
        for p in params {
            rpc_params.insert(p).unwrap();
        }

        let sub: Subscription<Value> = client
            .subscribe("eth_subscribe", rpc_params, "eth_unsubscribe")
            .await
            .unwrap_or_else(|e| panic!("subscribe({kind}) should succeed: {e}"));

        sub.unsubscribe()
            .await
            .unwrap_or_else(|e| panic!("unsubscribe({kind}) should succeed: {e}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_syncing_delivers_initial_status() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws_eth().await;
    let client = handle.ws_client().await.unwrap();

    let mut sub: Subscription<Value> = client
        .subscribe("eth_subscribe", jsonrpsee::rpc_params!["syncing"], "eth_unsubscribe")
        .await
        .unwrap();

    let initial = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for initial sync status")
        .expect("subscription ended unexpectedly")
        .expect("failed to deserialize sync status");

    // NoopNetwork reports is_syncing = false
    assert_eq!(initial, serde_json::json!(false));

    sub.unsubscribe().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_invalid_kind_rejected() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws_eth().await;
    let client = handle.ws_client().await.unwrap();

    let result: Result<Subscription<Value>, _> = client
        .subscribe("eth_subscribe", jsonrpsee::rpc_params!["invalidKind"], "eth_unsubscribe")
        .await;

    assert!(result.is_err(), "invalid subscription kind must be rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_server_survives_client_disconnect() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws_eth().await;

    {
        let client = handle.ws_client().await.unwrap();
        let _sub: Subscription<Value> = client
            .subscribe("eth_subscribe", jsonrpsee::rpc_params!["newHeads"], "eth_unsubscribe")
            .await
            .unwrap();
        // client + subscription drop here
    }

    // Server must still accept new connections after a client disconnects
    let client2 = handle.ws_client().await.unwrap();
    let sub: Subscription<Value> = client2
        .subscribe("eth_subscribe", jsonrpsee::rpc_params!["newHeads"], "eth_unsubscribe")
        .await
        .unwrap();

    sub.unsubscribe().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_not_available_over_http() {
    base_common_observability_tracing::init_test_tracing();

    let mut registry = test_rpc_registry().await;
    let server = registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_http());
    let handle = RpcServerConfig::http(Default::default())
        .with_http_address(crate::utils::test_address())
        .start(&server)
        .await
        .unwrap();

    assert!(handle.ws_client().await.is_none(), "WS should not be available on HTTP-only server");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_subscribe_pending_transactions_receives_tx() {
    use base_common_runtime_tasks::Runtime;
    use base_execution_evm_blocks::BaseBeaconConsensus;
    use base_execution_txpool::{TransactionOrigin, TransactionPool};
    use reth_rpc_builder::RpcRegistryInner;

    base_common_observability_tracing::init_test_tracing();

    let signed = base_execution_txpool::test_utils::TransactionBuilder::default()
        .chain_id(8453)
        .nonce(0)
        .gas_limit(21_000)
        .value(0)
        .to(alloy_primitives::Address::repeat_byte(2))
        .max_fee_per_gas(2_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559();
    let recovered = signed.try_into_recovered().unwrap();
    let mock = base_execution_state_provider::test_utils::MockEthProvider::default();
    mock.add_account(
        recovered.signer(),
        base_execution_state_provider::test_utils::ExtendedAccount::new(0, alloy_primitives::U256::MAX),
    );
    let tx = base_execution_txpool::BasePooledTransaction::try_from_consensus(recovered).unwrap();
    let context = base_execution_rpc_handlers::test_utils::RpcTestUtils::context(mock);
    let pool_clone = context.pool.clone();
    let eth_api = base_execution_rpc_handlers::EthApiBuilder::new_with_components(context.clone()).build();
    let mut registry = RpcRegistryInner::new(
        context.provider,
        context.pool,
        context.network,
        Runtime::test(),
        std::sync::Arc::new(BaseBeaconConsensus::noop()),
        Default::default(),
        context.evm_config,
        eth_api,
        EventSender::new(1),
    );

    let server = registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_ws());
    let handle = RpcServerConfig::ws(Default::default())
        .with_ws_address(crate::utils::test_address())
        .start(&server)
        .await
        .unwrap();

    let client = handle.ws_client().await.unwrap();

    // Subscribe to pending transaction hashes
    let mut sub: Subscription<Value> = client
        .subscribe(
            "eth_subscribe",
            jsonrpsee::rpc_params!["newPendingTransactions"],
            "eth_unsubscribe",
        )
        .await
        .unwrap();

    let expected_hash = *tx.hash();
    pool_clone.add_transaction(TransactionOrigin::External, tx).await.unwrap();

    // We should receive the tx hash via the subscription
    let received = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for pending tx notification")
        .expect("subscription ended unexpectedly")
        .expect("failed to deserialize tx hash");

    let received_hash: alloy_primitives::TxHash = serde_json::from_value(received).unwrap();
    assert_eq!(received_hash, expected_hash);

    sub.unsubscribe().await.unwrap();
}
