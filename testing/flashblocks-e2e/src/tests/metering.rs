//! Tests for metering RPC endpoints.
//!
//! These tests require the node to support the `base_meterBundle` RPC method.
//! Not all nodes have this.

use alloy_eips::BlockNumberOrTag;
use eyre::{Result, ensure};

use crate::{
    TestClient,
    tests::{Test, TestCategory},
    types::Bundle,
};

/// Check if the node supports metering RPC methods.
async fn check_metering_support(client: &TestClient) -> Option<String> {
    // Try a simple metering call - if it returns "Method not found", skip
    let bundle = Bundle { block_number: 1, ..Default::default() };
    match client.meter_bundle(bundle).await {
        Ok(_) => None,
        Err(e) => {
            let err_str = format!("{:?}", e);
            if err_str.contains("-32601") || err_str.contains("Method not found") {
                Some("Node does not support base_meterBundle RPC method".to_string())
            } else {
                // Some other error - let the test run and fail with details
                None
            }
        }
    }
}

/// Build the metering test category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "metering".to_string(),
        description: Some(
            "Bundle metering and priority fee estimation tests (requires base_meterBundle support)"
                .to_string(),
        ),
        tests: vec![
            Test {
                name: "meter_bundle_empty".to_string(),
                description: Some("Meter an empty bundle".to_string()),
                run: Box::new(|client| Box::pin(test_meter_bundle_empty(client))),
                skip_if: Some(Box::new(|client| Box::pin(check_metering_support(client)))),
            },
            Test {
                name: "meter_bundle_state_block".to_string(),
                description: Some("Verify metering returns valid state block".to_string()),
                run: Box::new(|client| Box::pin(test_meter_bundle_state_block(client))),
                skip_if: Some(Box::new(|client| Box::pin(check_metering_support(client)))),
            },
        ],
    }
}

async fn test_meter_bundle_empty(client: &TestClient) -> Result<()> {
    let bundle = Bundle { block_number: 1, ..Default::default() };

    let response = client.meter_bundle(bundle).await?;

    ensure!(response.results.is_empty(), "Empty bundle should have no results");
    ensure!(response.total_gas_used == 0, "Empty bundle should use 0 gas");

    tracing::debug!(state_block = response.state_block_number, "Metered empty bundle");

    Ok(())
}

async fn test_meter_bundle_state_block(client: &TestClient) -> Result<()> {
    let bundle = Bundle { block_number: 1, ..Default::default() };

    let response = client.meter_bundle(bundle).await?;

    // state_block_number should be a valid block number
    tracing::debug!(state_block = response.state_block_number, "State block from metering");

    // Verify the state block exists
    let block =
        client.get_block_by_number(BlockNumberOrTag::Number(response.state_block_number)).await?;

    ensure!(block.is_some(), "State block should exist");

    Ok(())
}
