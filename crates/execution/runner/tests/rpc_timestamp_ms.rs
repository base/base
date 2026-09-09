//! End-to-end wire checks for canonical Cobalt RPC timestamps.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_eips::Encodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_client::RpcClient;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{Account, build_test_genesis};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const BLOCK_NUMBER: u64 = 1;
const TIMESTAMP_MILLIS_PART: u16 = 200;
const TIMESTAMP_MS_QUANTITY: &str = "0xc80";

async fn request(client: &RpcClient, method: &'static str, params: Value) -> eyre::Result<Value> {
    Ok(client.request(method, params).await?)
}

fn assert_quantity(response: &Value, field: &str) {
    assert_eq!(response[field], TIMESTAMP_MS_QUANTITY, "missing or incorrect {field}");
}

fn assert_log_quantities(logs: &Value) {
    let logs = logs.as_array().expect("logs response should be an array");
    assert!(!logs.is_empty(), "logs response should not be empty");
    for log in logs {
        assert_quantity(log, "blockTimestampMs");
    }
}

fn receipt_logs(receipts: &Value, transaction_hash: &str) -> Value {
    let receipts = receipts.as_array().expect("receipts response should be an array");
    receipts
        .iter()
        .find(|receipt| receipt["transactionHash"] == transaction_hash)
        .expect("log-emitting transaction receipt should be present")["logs"]
        .clone()
}

#[tokio::test]
async fn canonical_cobalt_rpc_responses_include_millisecond_timestamps() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "cobalt": 3 }));
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .build()
        .await?;
    let client = harness.rpc_client()?;

    let filter_id = request(&client, "eth_newFilter", json!([{}])).await?;
    let (mut ws, _) = connect_async(harness.ws_url()).await?;
    for (id, kind) in [(1, "newHeads"), (2, "logs"), (3, "transactionReceipts")] {
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "eth_subscribe",
                "params": [kind],
            })
            .to_string()
            .into(),
        ))
        .await?;
    }
    let mut subscriptions = HashMap::new();
    while subscriptions.len() < 3 {
        let response: Value = serde_json::from_str(ws.next().await.unwrap()?.to_text()?)?;
        let kind = match response["id"].as_u64() {
            Some(1) => "newHeads",
            Some(2) => "logs",
            Some(3) => "transactionReceipts",
            _ => continue,
        };
        subscriptions.insert(response["result"].as_str().unwrap().to_owned(), kind);
    }

    let base_time = BaseTimeUpdateTx::new(TIMESTAMP_MILLIS_PART)?.into_deposit_tx(BLOCK_NUMBER);
    let transaction_hash = base_time.hash();
    let (log_transaction, _, log_transaction_hash) = Account::Deployer
        .create_deployment_tx(Bytes::from_static(&[0x60, 0, 0x60, 0, 0xa0, 0]), 0)?;
    harness
        .build_block_from_transactions(vec![
            L1_BLOCK_INFO_DEPOSIT_TX,
            base_time.encoded_2718().into(),
            log_transaction,
        ])
        .await?;
    let block = harness.latest_block();
    let block_hash = block.hash();
    let block_number = format!("0x{:x}", block.number);

    for (method, params) in [
        ("eth_getBlockByHash", json!([block_hash, false])),
        ("eth_getBlockByNumber", json!([block_number, false])),
        ("eth_getHeaderByHash", json!([block_hash])),
        ("eth_getHeaderByNumber", json!([block_number])),
    ] {
        assert_quantity(&request(&client, method, params).await?, "timestampMs");
    }

    for (method, params) in [
        ("eth_getTransactionByHash", json!([transaction_hash])),
        ("eth_getTransactionByBlockHashAndIndex", json!([block_hash, "0x1"])),
        ("eth_getTransactionByBlockNumberAndIndex", json!([block_number, "0x1"])),
    ] {
        assert_quantity(&request(&client, method, params).await?, "blockTimestampMs");
    }

    for (method, params) in [
        ("eth_getBlockByHash", json!([block_hash, true])),
        ("eth_getBlockByNumber", json!([block_number, true])),
    ] {
        let block = request(&client, method, params).await?;
        assert_quantity(&block["transactions"][1], "blockTimestampMs");
    }

    let log_filter = json!([{ "fromBlock": block_number, "toBlock": block_number }]);
    assert_log_quantities(&request(&client, "eth_getLogs", log_filter).await?);
    assert_log_quantities(&request(&client, "eth_getFilterChanges", json!([filter_id])).await?);
    assert_log_quantities(&request(&client, "eth_getFilterLogs", json!([filter_id])).await?);

    let log_transaction_hash = format!("{log_transaction_hash:#x}");
    let receipt =
        request(&client, "eth_getTransactionReceipt", json!([log_transaction_hash])).await?;
    assert_log_quantities(&receipt["logs"]);
    let receipts = request(&client, "eth_getBlockReceipts", json!([block_number])).await?;
    assert_log_quantities(&receipt_logs(&receipts, &log_transaction_hash));

    let mut notifications = HashMap::new();
    while notifications.len() < 3 {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await?
            .ok_or_else(|| eyre::eyre!("WebSocket closed before all notifications arrived"))??;
        let notification: Value = serde_json::from_str(message.to_text()?)?;
        let Some(kind) =
            notification["params"]["subscription"].as_str().and_then(|id| subscriptions.get(id))
        else {
            continue;
        };
        notifications.insert(*kind, notification["params"]["result"].clone());
    }
    assert_quantity(&notifications["newHeads"], "timestampMs");
    assert_quantity(&notifications["logs"], "blockTimestampMs");
    assert_log_quantities(&receipt_logs(
        &notifications["transactionReceipts"],
        &log_transaction_hash,
    ));

    Ok(())
}
