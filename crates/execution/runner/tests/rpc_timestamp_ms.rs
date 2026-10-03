//! End-to-end wire checks for canonical and pending Denim RPC timestamps.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, U256};
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
const TIMESTAMP_MS_QUANTITY: &str = "0x4b0";
/// Returns `TIMESTAMP` and `NUMBER`.
const ENVIRONMENT_CODE: &str = "0x426000524360205260406000f3";
/// Reverts unless `TIMESTAMP` equals the first calldata word.
const TIMESTAMP_GUARD_CODE: &str = "0x4260003514600d5760006000fd5b00";

async fn request(client: &RpcClient, method: &'static str, params: Value) -> eyre::Result<Value> {
    Ok(client.request(method, params).await?)
}

fn words(output: &Value) -> Vec<u64> {
    let output: Bytes =
        serde_json::from_value(output.clone()).expect("call output should be bytes");
    output.chunks(32).map(|word| U256::from_be_slice(word).to::<u64>()).collect()
}

/// Executes `code` at `tag` and returns its output words.
async fn call_code(
    client: &RpcClient,
    code: &str,
    tag: &str,
    block_overrides: Value,
) -> eyre::Result<Vec<u64>> {
    let probe = Account::Alice.address();
    let state_overrides = json!({ (probe.to_string()): { "code": code } });
    let params = json!([{ "to": probe }, tag, state_overrides, block_overrides]);
    Ok(words(&request(client, "eth_call", params).await?))
}

/// Asserts that a pending estimate passes `guard` only with `expected` calldata, and that the
/// estimated gas suffices in the same context.
async fn assert_pending_estimate(
    client: &RpcClient,
    guard: &str,
    expected: u64,
    stale: u64,
) -> eyre::Result<()> {
    let probe = Account::Bob.address();
    let overrides = json!({ (probe.to_string()): { "code": guard } });
    let call = |word: u64| json!({ "to": probe, "data": format!("0x{word:064x}") });

    let gas =
        request(client, "eth_estimateGas", json!([call(expected), "pending", overrides])).await?;
    let mut exact = call(expected);
    exact["gas"] = gas;
    request(client, "eth_call", json!([exact, "pending", overrides])).await?;

    let stale: Result<Value, _> =
        client.request("eth_estimateGas", json!([call(stale), "pending", overrides])).await;
    assert_eq!(
        stale.unwrap_err().as_error_resp().map(|error| error.code),
        Some(3),
        "pending estimate must observe {expected}"
    );
    Ok(())
}

/// Returns block `number`'s system transactions, including the `BaseTime` deposit after Denim.
fn block_transactions(harness: &TestHarness, number: u64) -> eyre::Result<Vec<Bytes>> {
    let schedule = harness.chain_spec().denim_timestamp_schedule()?.expect("Denim is scheduled");
    let mut transactions = vec![L1_BLOCK_INFO_DEPOSIT_TX];
    if schedule.is_denim_active_at_block(number) {
        let millis_part = schedule.block_timestamp_parts(number).1;
        let base_time = BaseTimeUpdateTx::new(millis_part)?.into_deposit_tx(number);
        transactions.push(base_time.encoded_2718().into());
    }
    Ok(transactions)
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
async fn pending_denim_timestamp_is_independent_of_request_order() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 0 }));
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .build()
        .await?;
    let client = harness.rpc_client()?;
    let base_time = BaseTimeUpdateTx::new(TIMESTAMP_MILLIS_PART)?.into_deposit_tx(BLOCK_NUMBER);
    let prepared = harness
        .prepare_unsafe_block(vec![L1_BLOCK_INFO_DEPOSIT_TX, base_time.encoded_2718().into()])
        .await?;

    let cold =
        request(&client, "eth_getTransactionByBlockNumberAndIndex", json!(["pending", "0x1"]))
            .await?;
    assert_eq!(cold["blockHash"], json!(prepared.new_block_hash));
    assert_eq!(cold["hash"], json!(base_time.hash()));

    // Even a hashes-only block request warms the timestamp cache.
    let block = request(&client, "eth_getBlockByNumber", json!(["pending", false])).await?;
    assert_eq!(block["hash"], json!(prepared.new_block_hash));
    assert_quantity(&block, "timestampMs");
    let warm =
        request(&client, "eth_getTransactionByBlockNumberAndIndex", json!(["pending", "0x1"]))
            .await?;
    assert_quantity(&warm, "blockTimestampMs");
    assert_eq!(cold, warm, "a block request must not change pending transaction metadata");
    assert_quantity(&cold, "blockTimestampMs");

    // Neither RPC request is allowed to advance forkchoice.
    let latest = request(&client, "eth_getBlockByNumber", json!(["latest", false])).await?;
    assert_eq!(latest["hash"], json!(prepared.parent_hash));
    harness.engine().update_forkchoice(prepared.parent_hash, prepared.new_block_hash, None).await?;
    harness.wait_for_header(prepared.new_block_hash, prepared.new_block_number).await?;
    let canonical =
        request(&client, "eth_getTransactionByBlockNumberAndIndex", json!(["latest", "0x1"]))
            .await?;
    assert_eq!(canonical["hash"], cold["hash"]);
    assert_quantity(&canonical, "blockTimestampMs");
    Ok(())
}

#[tokio::test]
async fn canonical_denim_rpc_responses_include_millisecond_timestamps() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 0 }));
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

#[tokio::test]
async fn pending_forecast_follows_the_block_schedule() -> eyre::Result<()> {
    // Genesis is block 0 at 1s with a two-second legacy interval. Denim activates with block 2
    // at 5s, after which blocks advance by 200ms.
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 5 }));
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .build()
        .await?;
    let client = harness.rpc_client()?;

    // Latest block and its successor's seconds: legacy, first Denim block, same second, and
    // second rollover.
    for (latest, seconds) in [(0, 3), (1, 5), (4, 5), (6, 6)] {
        while harness.latest_block().number < latest {
            let number = harness.latest_block().number + 1;
            harness.build_block_from_transactions(block_transactions(&harness, number)?).await?;
        }

        assert_eq!(
            call_code(&client, ENVIRONMENT_CODE, "pending", Value::Null).await?,
            [seconds, latest + 1]
        );
        assert_pending_estimate(&client, TIMESTAMP_GUARD_CODE, seconds, seconds - 1).await?;
        // Latest keeps its own context.
        assert_eq!(
            call_code(&client, ENVIRONMENT_CODE, "latest", Value::Null).await?,
            [harness.latest_block().timestamp, latest]
        );
    }

    // User block overrides take precedence over the forecast.
    let block_override = json!({ "time": "0x9", "number": "0x7" });
    assert_eq!(call_code(&client, ENVIRONMENT_CODE, "pending", block_override).await?, [9, 7]);

    // An executed pending block is used as-is rather than advanced again.
    let prepared = harness.prepare_unsafe_block(block_transactions(&harness, 7)?).await?;
    assert_eq!(call_code(&client, ENVIRONMENT_CODE, "pending", Value::Null).await?, [6, 7]);

    // Once forkchoice consumes it, pending forecasts its successor.
    harness.engine().update_forkchoice(prepared.parent_hash, prepared.new_block_hash, None).await?;
    harness.wait_for_header(prepared.new_block_hash, prepared.new_block_number).await?;
    assert_eq!(call_code(&client, ENVIRONMENT_CODE, "pending", Value::Null).await?, [6, 8]);
    Ok(())
}
