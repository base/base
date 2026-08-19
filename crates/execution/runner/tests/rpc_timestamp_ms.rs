//! End-to-end wire checks for canonical Denim block and transaction timestamps.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_rpc_client::RpcClient;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::build_test_genesis;
use serde_json::{Value, json};

const BLOCK_NUMBER: u64 = 1;
const TIMESTAMP_MILLIS_PART: u16 = 200;
const TIMESTAMP_MS_QUANTITY: &str = "0xc80";

async fn request(client: &RpcClient, method: &'static str, params: Value) -> eyre::Result<Value> {
    Ok(client.request(method, params).await?)
}

fn assert_quantity(response: &Value, field: &str) {
    assert_eq!(response[field], TIMESTAMP_MS_QUANTITY, "missing or incorrect {field}");
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

    let base_time = BaseTimeUpdateTx::new(TIMESTAMP_MILLIS_PART)?.into_deposit_tx(BLOCK_NUMBER);
    let transaction_hash = base_time.hash();
    harness
        .build_block_from_transactions(vec![
            L1_BLOCK_INFO_DEPOSIT_TX,
            base_time.encoded_2718().into(),
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

    Ok(())
}
