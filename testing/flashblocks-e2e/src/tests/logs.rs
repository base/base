//! Tests for eth_getLogs.

use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_eth::Filter;
use eyre::Result;

use crate::{
    TestClient,
    tests::{Test, TestCategory},
};

/// Build the logs test category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "logs".to_string(),
        description: Some("eth_getLogs tests including pending logs".to_string()),
        tests: vec![
            Test {
                name: "get_logs_latest".to_string(),
                description: Some("Get logs from latest block".to_string()),
                run: Box::new(|client| Box::pin(test_get_logs_latest(client))),
                skip_if: None,
            },
            Test {
                name: "get_logs_pending".to_string(),
                description: Some("Get logs including pending block".to_string()),
                run: Box::new(|client| Box::pin(test_get_logs_pending(client))),
                skip_if: None,
            },
            Test {
                name: "get_logs_range".to_string(),
                description: Some("Get logs from a block range".to_string()),
                run: Box::new(|client| Box::pin(test_get_logs_range(client))),
                skip_if: None,
            },
            Test {
                name: "get_logs_mixed_range".to_string(),
                description: Some("Get logs from fromBlock: 0, toBlock: pending".to_string()),
                run: Box::new(|client| Box::pin(test_get_logs_mixed_range(client))),
                skip_if: None,
            },
        ],
    }
}

async fn test_get_logs_latest(client: &TestClient) -> Result<()> {
    let filter = Filter::new().select(BlockNumberOrTag::Latest);

    let logs = client.get_logs(&filter).await?;
    tracing::debug!(count = logs.len(), "Got logs at latest block");

    Ok(())
}

async fn test_get_logs_pending(client: &TestClient) -> Result<()> {
    // Query logs from pending to pending (flashblocks state only)
    let filter =
        Filter::new().from_block(BlockNumberOrTag::Pending).to_block(BlockNumberOrTag::Pending);

    let logs = client.get_logs(&filter).await?;
    tracing::debug!(count = logs.len(), "Got pending logs");

    Ok(())
}

async fn test_get_logs_range(client: &TestClient) -> Result<()> {
    // Get the latest block number
    let latest_block = client
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("No latest block"))?;

    let from_block = latest_block.header.number.saturating_sub(10);

    let filter = Filter::new().from_block(from_block).to_block(BlockNumberOrTag::Latest);

    let logs = client.get_logs(&filter).await?;
    tracing::debug!(
        count = logs.len(),
        from = from_block,
        to = latest_block.header.number,
        "Got logs in range"
    );

    Ok(())
}

async fn test_get_logs_mixed_range(client: &TestClient) -> Result<()> {
    // Get the latest block number
    let latest_block = client
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("No latest block"))?;

    let from_block = latest_block.header.number.saturating_sub(10);

    // Query from historical to pending (should include both canonical and flashblocks logs)
    let filter = Filter::new().from_block(from_block).to_block(BlockNumberOrTag::Pending);

    let logs = client.get_logs(&filter).await?;
    tracing::debug!(count = logs.len(), from = from_block, "Got logs from historical to pending");

    Ok(())
}
