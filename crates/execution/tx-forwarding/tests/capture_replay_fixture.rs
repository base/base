//! Opt-in wire capture through the production forwarder, without a devnet or recorder server.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::keccak256;
use alloy_signer::SignerSync;
use axum::{Json, Router, body::Bytes, routing::post};
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use base_test_utils::{Account, DEVNET_CHAIN_ID};
use base_tx_forwarding::{InsertValidatedTransaction, TxForwardingConfig, TxForwardingService};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc, task::JoinSet, time::timeout};
use url::Url;

#[tokio::test]
#[ignore = "writes captured HTTP request to REPLAY_FIXTURE_PATH"]
async fn capture_insert_validated_transaction() {
    let path = std::env::var("REPLAY_FIXTURE_PATH").expect("set explicit output path");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: Url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
    let (captured, mut requests) = mpsc::channel(1);
    let app = Router::new().route(
        "/",
        post(move |body: Bytes| {
            let captured = captured.clone();
            async move {
                let requests: Vec<Value> = serde_json::from_slice(&body).unwrap();
                let response: Vec<_> = requests
                    .iter()
                    .map(|request| json!({"jsonrpc": "2.0", "id": request["id"], "result": null}))
                    .collect();
                captured.send(body).await.unwrap();
                Json(response)
            }
        }),
    );
    let mut server = JoinSet::new();
    server.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let transaction = BaseTransactionRequest::default()
        .from(Account::Alice.address())
        .transaction_type(2u8)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .to(Account::Bob.address())
        .with_nonce(0)
        .build_typed_tx()
        .unwrap();
    let signature = Account::Alice.signer().sign_hash_sync(&transaction.signature_hash()).unwrap();
    let raw = transaction.into_signed(signature).encoded_2718();
    let request = InsertValidatedTransaction {
        tx_hash: keccak256(&raw),
        transaction: ValidatedTransaction {
            sender: Account::Alice.address(),
            raw: raw.into(),
            metering: None,
            extensions: NoExtensions {},
        },
    };
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()
    .unwrap();
    let (sender, receiver) = mpsc::channel(1);
    sender.send(request).await.unwrap();
    drop(sender);

    let forwarder = TxForwardingService::new(TxForwardingConfig::new(vec![url.clone()]))
        .spawn_requests(vec![(url, receiver)], &runtime)
        .unwrap();
    let body = timeout(Duration::from_secs(10), requests.recv()).await.unwrap().unwrap();
    let report = timeout(Duration::from_secs(10), forwarder.shutdown()).await.unwrap();

    assert_eq!(report.forwarders_completed, 1);
    assert_eq!(report.task_failures, 0);
    assert!(!report.timed_out);
    let requests: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "base_insertValidatedTransaction");
    std::fs::write(&path, &body).unwrap();
    println!("Captured {} HTTP body bytes from TxForwardingService into {path}", body.len());
    server.shutdown().await;
}
