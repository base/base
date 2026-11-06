//! Wire checks for RPC gas reporting under gas-limit billing (`BaseFeature::GasLimitBilling`,
//! active from Everest): `eth_estimateGas` and unvalidated `eth_simulateV1` report the gas a
//! call meters, while `eth_simulateV1` with `validation=true` reports the billed limit.

use std::sync::Arc;

use alloy_primitives::U256;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, build_test_genesis_everest};
use serde_json::{Value, json};

/// Gas a plain value transfer meters.
const TRANSFER_GAS: u64 = 21_000;
/// Gas limit given to each call of a validated simulation, far above what the probe meters.
const CALL_GAS_LIMIT: u64 = 200_000;
/// `PUSH1 1, PUSH1 0, SSTORE, STOP`: writes slot 0 so the call meters well above a transfer.
const SSTORE_CODE: &str = "0x600160005500";
/// Gas the probe meters when slot 0 is zero: intrinsic plus two `PUSH1` and a zero-to-nonzero
/// `SSTORE` on a cold slot (20 000 + 2 100).
const SSTORE_COLD_CALL_GAS: u64 = TRANSFER_GAS + 3 + 3 + 20_000 + 2_100;
/// Gas the probe meters once slot 0 already holds 1: the `SSTORE` is a no-op (100) on a slot
/// that is cold again, since access warmth does not carry across the block's transactions.
const SSTORE_NOOP_CALL_GAS: u64 = TRANSFER_GAS + 3 + 3 + 2_100 + 100;
/// Upper bound on the binary-searched estimate of a call metering `SSTORE_COLD_CALL_GAS`:
/// reth stops searching within `ESTIMATE_GAS_ERROR_RATIO` (1.5%) of the metered gas.
const SSTORE_COLD_ESTIMATE_CEILING: u64 = SSTORE_COLD_CALL_GAS + SSTORE_COLD_CALL_GAS * 15 / 1000;

async fn everest_harness() -> eyre::Result<TestHarness> {
    TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(build_test_genesis_everest())))
        .build()
        .await
}

/// A call from Alice to the probe deployed at Bob's address, with an optional gas limit.
fn probe_call(gas: Option<u64>) -> Value {
    let mut call = json!({ "from": Account::Alice.address(), "to": Account::Bob.address() });
    if let Some(gas) = gas {
        call["gas"] = json!(format!("0x{gas:x}"));
    }
    call
}

fn gas_used(call: &Value) -> u64 {
    let gas: U256 = serde_json::from_value(call["gasUsed"].clone()).expect("gasUsed is hex");
    gas.to::<u64>()
}

/// Estimates stay metered under gas-limit billing: a transfer estimates its intrinsic gas and
/// a contract call the gas its code meters, neither the block gas limit nor a padded limit.
#[tokio::test]
async fn estimate_gas_reports_metered_gas_under_gas_limit_billing() -> eyre::Result<()> {
    let harness = everest_harness().await?;
    let client = harness.rpc_client()?;
    let alice = Account::Alice.address();
    let bob = Account::Bob.address();

    let transfer = json!({ "from": alice, "to": bob, "value": "0x1" });
    let gas: U256 = client.request("eth_estimateGas", json!([transfer, "latest"])).await?;
    assert_eq!(gas, U256::from(TRANSFER_GAS));

    let overrides = json!({ bob.to_string(): { "code": SSTORE_CODE } });
    let call = json!({ "from": alice, "to": bob, "gas": format!("0x{CALL_GAS_LIMIT:x}") });
    let gas: U256 = client.request("eth_estimateGas", json!([call, "latest", overrides])).await?;
    let gas = gas.to::<u64>();
    assert!(
        (SSTORE_COLD_CALL_GAS..=SSTORE_COLD_ESTIMATE_CEILING).contains(&gas),
        "the estimate must track the metered gas, not the {CALL_GAS_LIMIT} limit: {gas}"
    );
    Ok(())
}

/// An unvalidated simulation bills nothing, like `eth_call`: every call reports the gas it
/// metered, so a call without `gas` neither consumes the block's remaining gas nor drains the
/// request-wide `--rpc.gascap` budget, and the calls after it still run.
#[tokio::test]
async fn unvalidated_simulation_reports_metered_gas_under_gas_limit_billing() -> eyre::Result<()> {
    let harness = everest_harness().await?;
    let client = harness.rpc_client()?;
    let overrides = json!({ Account::Bob.address().to_string(): { "code": SSTORE_CODE } });

    let response: Value = client
        .request(
            "eth_simulateV1",
            json!([{
                "validation": false,
                "blockStateCalls": [
                    { "stateOverrides": overrides, "calls": [probe_call(None), probe_call(None)] },
                ]
            }, "latest"]),
        )
        .await?;

    let calls = response[0]["calls"].as_array().expect("one simulated block with calls");
    assert_eq!(calls.len(), 2, "both calls must execute: {response}");
    for call in calls {
        assert_eq!(call["status"], "0x1", "call must succeed: {call}");
    }
    // Both calls defaulted to the block's remaining gas yet report only what they metered.
    assert_eq!(gas_used(&calls[0]), SSTORE_COLD_CALL_GAS);
    assert_eq!(gas_used(&calls[1]), SSTORE_NOOP_CALL_GAS);
    assert_eq!(gas_used(&response[0]), SSTORE_COLD_CALL_GAS + SSTORE_NOOP_CALL_GAS);
    Ok(())
}

/// A validated simulation reports what a receipt would: under gas-limit billing each call is
/// accounted at its gas limit however little it meters.
#[tokio::test]
async fn validated_simulation_reports_the_gas_limit_under_gas_limit_billing() -> eyre::Result<()> {
    let harness = everest_harness().await?;
    let client = harness.rpc_client()?;
    let overrides = json!({ Account::Bob.address().to_string(): { "code": SSTORE_CODE } });
    // Validation enforces the base fee, so price the calls like real transactions.
    let mut call = probe_call(Some(CALL_GAS_LIMIT));
    call["maxFeePerGas"] = json!("0x3b9aca00");
    let calls = [call.clone(), call];

    let response: Value = client
        .request(
            "eth_simulateV1",
            json!([{
                "validation": true,
                "blockStateCalls": [{ "stateOverrides": overrides, "calls": calls }]
            }, "latest"]),
        )
        .await?;

    let calls = response[0]["calls"].as_array().expect("one simulated block with calls");
    assert_eq!(calls.len(), 2, "both calls must execute: {response}");
    for call in calls {
        assert_eq!(call["status"], "0x1", "call must succeed: {call}");
        assert_eq!(gas_used(call), CALL_GAS_LIMIT, "validated calls are billed their limit");
    }
    assert_eq!(gas_used(&response[0]), 2 * CALL_GAS_LIMIT);
    Ok(())
}
