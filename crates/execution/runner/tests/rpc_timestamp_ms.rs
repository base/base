//! End-to-end wire checks for canonical and pending Denim RPC timestamps.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_eips::{Encodable2718, eip1559::BaseFeeParams};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_client::RpcClient;
use base_common_consensus::{JovianExtraData, Predeploys};
use base_common_evm::BaseTime;
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
async fn pending_calls_use_the_selected_blocks_environment_and_state() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 3 }));
    // The general test genesis has placeholder extra data; successor fees need valid parameters.
    genesis.extra_data = JovianExtraData::encode([0; 8].into(), BaseFeeParams::new(50, 6), 0)?;
    genesis.alloc.remove(&BaseTime::IMPLEMENTATION_ADDRESS);
    genesis
        .alloc
        .get_mut(&Predeploys::BASE_TIME)
        .unwrap()
        .storage
        .as_mut()
        .unwrap()
        .remove(&B256::from(BaseTime::IMPLEMENTATION_SLOT));
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .build()
        .await?;
    let client = harness.rpc_client()?;
    let probe = Account::Alice.address();
    // Return TIMESTAMP, NUMBER, and BASEFEE, in that order.
    let environment_code = "0x42600052436020524860405260606000f3";
    // STATICCALL BaseTime.timestampMs() and revert unless it equals the first calldata word.
    let time_guard_code = concat!(
        "0x635745a67760e01b600052602060006004600073",
        "4200000000000000000000000000000000000030",
        "5afa5060005160003514603a5760006000fd5b00"
    );

    let base_time_call = |tag| json!([{ "to": Predeploys::BASE_TIME, "data": "0x5745a677" }, tag]);

    // Without an executed pending block, simulate the scheduled successor. Once a payload
    // exists, use its environment and post-state until forkchoice consumes it.
    for stage in 0..3 {
        let block = request(&client, "eth_getBlockByNumber", json!(["pending", false])).await?;
        let timestamp = 3_u64;
        let number = if stage == 2 { 2_u64 } else { 1 };
        // Empty genesis reduces 1 gwei by 1/50. The harness's first payload then sets a
        // 1 gwei minimum in its extra data, which takes effect for its successor.
        let base_fee = [980_000_000_u64, 980_000_000, 1_000_000_000][stage];
        let output = request(
            &client,
            "eth_call",
            json!([
                { "from": Account::Bob.address(), "to": probe, "gas": "0x186a0", "gasPrice": block["baseFeePerGas"] },
                "pending",
                { (probe.to_string()): { "code": environment_code } }
            ]),
        )
        .await?;
        assert_eq!(
            output,
            json!(format!("0x{timestamp:064x}{number:064x}{base_fee:064x}")),
            "stage {stage}"
        );

        let timestamp_ms = [3_000_u64, 3_600, 3_200][stage];
        let output = request(
            &client,
            "eth_call",
            json!([{ "to": Predeploys::BASE_TIME, "data": "0x5745a677" }, "pending"]),
        )
        .await?;
        assert_eq!(output, json!(format!("0x{timestamp_ms:064x}")), "stage {stage}");

        let call =
            json!({ "to": probe, "data": format!("0x{timestamp_ms:064x}"), "gas": "0x186a0" });
        let overrides = json!({ (probe.to_string()): { "code": time_guard_code } });
        let gas = request(&client, "eth_estimateGas", json!([call, "pending", overrides])).await?;
        assert!(u64::from_str_radix(gas.as_str().unwrap().trim_start_matches("0x"), 16)? > 21_000);
        let stale_timestamp_ms = [1_000, 3_000, 3_600][stage];
        let wrong_time = json!({
            "to": probe,
            "data": format!("0x{stale_timestamp_ms:064x}"),
            "gas": "0x186a0"
        });
        let result: Result<Value, _> =
            client.request("eth_estimateGas", json!([wrong_time, "pending", overrides])).await;
        assert_eq!(
            result.unwrap_err().as_error_resp().unwrap().code,
            3,
            "the mismatched timestamp must revert"
        );

        // Protocol initialization supplies the temporary state first; user state overrides win.
        let overridden = request(
            &client,
            "eth_call",
            json!([
                { "to": Predeploys::BASE_TIME, "data": "0x5745a677" },
                "pending",
                { (Predeploys::BASE_TIME.to_string()): {
                    "stateDiff": {
                        "0x0000000000000000000000000000000000000000000000000000000000000000":
                            "0x000000000000000000000000000000000000000000000000000000000000012c"
                    }
                } }
            ]),
        )
        .await?;
        assert_eq!(overridden, json!(format!("0x{:064x}", timestamp * 1_000 + 300)));

        let block_overridden = request(
            &client,
            "eth_call",
            json!([
                { "from": Account::Bob.address(), "to": probe, "gas": "0x186a0", "gasPrice": "0xb" },
                "pending",
                { (probe.to_string()): { "code": environment_code } },
                { "timestamp": "0x9", "number": "0x7", "baseFeePerGas": "0xb" }
            ]),
        )
        .await?;
        assert_eq!(block_overridden, json!(format!("0x{:064x}{:064x}{:064x}", 9, 7, 11)));

        for tag in ["0x0", "latest"] {
            let result: Result<Value, _> = client.request("eth_call", base_time_call(tag)).await;
            if tag == "latest" && stage == 2 {
                assert_eq!(result?, json!(format!("0x{:064x}", 3_600)));
            } else {
                let error = result.unwrap_err();
                let error = error.as_error_resp().unwrap();
                assert_eq!(error.code, 3);
                assert!(error.message.contains("implementation not initialized"));
            }
        }

        if stage == 0 {
            // Deliberately differ from the forecast: real pending must use executed metadata.
            let base_time = BaseTimeUpdateTx::new(600)?.into_deposit_tx(1);
            harness
                .prepare_unsafe_block(vec![
                    L1_BLOCK_INFO_DEPOSIT_TX,
                    base_time.encoded_2718().into(),
                ])
                .await?;
        } else if stage == 1 {
            // Promote the real pending block; the fallback must now forecast its successor.
            let hash = serde_json::from_value(block["hash"].clone())?;
            let parent = serde_json::from_value(block["parentHash"].clone())?;
            harness.engine().update_forkchoice(parent, hash, None).await?;
            harness.wait_for_header(hash, 1).await?;
        }
    }

    let historical = request(
        &client,
        "eth_call",
        json!([
            { "from": Account::Bob.address(), "to": probe, "gas": "0x186a0", "gasPrice": "0x3b9aca00" },
            "0x0",
            { (probe.to_string()): { "code": environment_code } }
        ]),
    )
    .await?;
    assert_eq!(historical, json!(format!("0x{:064x}{:064x}{:064x}", 1, 0, 1_000_000_000)));

    Ok(())
}

#[tokio::test]
async fn pending_denim_timestamp_is_independent_of_request_order() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 3 }));
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
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 3 }));
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
