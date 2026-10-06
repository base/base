//! Wire checks for timestamp-dependent simulation environments.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, build_test_genesis};
use serde_json::{Value, json};

#[tokio::test]
async fn simulation_selects_forks_at_the_simulated_timestamp() -> eyre::Result<()> {
    // Azul activates before or after the old genesis + 12 fallback, respectively.
    for activation in [10, 20] {
        let mut genesis = build_test_genesis();
        genesis.config.osaka_time = Some(activation);
        genesis.config.extra_fields.insert("base".into(), json!({ "azul": activation }));
        let harness = TestHarness::builder()
            .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
            .build()
            .await?;
        let client = harness.rpc_client()?;
        let probe = Account::Bob.address();
        // CLZ(1) returns 255 under Osaka and halts before Osaka. Also return TIMESTAMP.
        let code = "0x60011e6000524260205260406000f3";
        let call = json!({ "from": Account::Alice.address(), "to": probe, "gas": "0x186a0" });
        let timestamps = if activation == 10 { [3, 9, 10, 12] } else { [20, 21, 22, 24] };
        let first_override = if activation == 10 { Value::Null } else { json!({ "time": "0x14" }) };

        for trace_transfers in [false, true] {
            let response: Value = client.request("eth_simulateV1", json!([{
                "traceTransfers": trace_transfers,
                "blockStateCalls": [
                    { "blockOverrides": first_override, "stateOverrides": { (probe.to_string()): { "code": code } }, "calls": [call] },
                    { "blockOverrides": { "time": format!("0x{:x}", timestamps[1]) }, "calls": [call] },
                    { "blockOverrides": { "time": format!("0x{:x}", timestamps[2]) }, "calls": [call] },
                    { "calls": [call] }
                ]
            }, "latest"])).await?;

            let blocks = response.as_array().unwrap();
            assert_eq!(blocks.len(), 4);
            for (block, timestamp) in blocks.iter().zip(timestamps) {
                assert_eq!(block["timestamp"], json!(format!("0x{timestamp:x}")));
                let result = &block["calls"][0];
                if timestamp < activation {
                    assert_eq!(result["status"], "0x0", "CLZ must be disabled at {timestamp}");
                    assert!(!result["error"].is_null());
                } else {
                    assert_eq!(result["status"], "0x1", "CLZ must be enabled at {timestamp}");
                    assert_eq!(
                        result["returnData"],
                        json!(format!("0x{:064x}{timestamp:064x}", 255))
                    );
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn simulation_preserves_same_second_denim_timestamps_and_overrides() -> eyre::Result<()> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 3 }));
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .build()
        .await?;
    let client = harness.rpc_client()?;
    let probe = Account::Bob.address();
    // Return TIMESTAMP and BASEFEE. Explicit nonzero gas price keeps BASEFEE observable.
    let code = "0x426000524860205260406000f3";
    let call = json!({ "from": Account::Alice.address(), "to": probe, "gas": "0x186a0", "gasPrice": "0x3b9aca00" });
    let response: Value = client
        .request(
            "eth_simulateV1",
            json!([{
        "blockStateCalls": [
            { "stateOverrides": { (probe.to_string()): { "code": code } }, "calls": [call] },
            { "blockOverrides": { "baseFeePerGas": "0x2a" }, "calls": [call] },
            { "blockOverrides": { "time": "0x4" }, "calls": [call] },
            { "calls": [call] }
        ]
    }, "latest"]),
        )
        .await?;
    let blocks = response.as_array().unwrap();
    assert_eq!(blocks.len(), 4);
    for (index, (timestamp, base_fee)) in [(3, 0), (3, 42), (4, 0), (4, 0)].into_iter().enumerate()
    {
        assert_eq!(blocks[index]["number"], json!(format!("0x{:x}", index + 1)));
        assert_eq!(blocks[index]["timestamp"], json!(format!("0x{timestamp:x}")));
        assert_eq!(blocks[index]["calls"][0]["status"], "0x1");
        assert_eq!(
            blocks[index]["calls"][0]["returnData"],
            json!(format!("0x{timestamp:064x}{base_fee:064x}"))
        );
    }
    Ok(())
}
