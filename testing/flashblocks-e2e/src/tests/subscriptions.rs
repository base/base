//! Sanity checks for the flashblocks WebSocket endpoint.
//!
//! These tests verify the flashblocks WebSocket is reachable and streaming correctly.
//! They do NOT test the RPC node - they test the external flashblocks endpoint
//! (--flashblocks-ws-url) which is a prerequisite for the flashblock-aware tests.

use std::time::Duration;

use eyre::{Result, ensure};

use crate::{
    TestClient,
    harness::FlashblocksStream,
    tests::{Test, TestCategory},
};

/// Build the sanity checks category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "sanity".to_string(),
        description: Some(
            "Sanity checks for flashblocks WebSocket (tests the endpoint, not the RPC node)"
                .to_string(),
        ),
        tests: vec![
            Test {
                name: "flashblocks_ws_connect".to_string(),
                description: Some("Verify flashblocks WebSocket is reachable".to_string()),
                run: Box::new(|client| Box::pin(test_flashblocks_stream_connect(client))),
                skip_if: None,
            },
            Test {
                name: "flashblocks_ws_receive".to_string(),
                description: Some("Verify flashblocks are being streamed".to_string()),
                run: Box::new(|client| Box::pin(test_flashblocks_stream_receive(client))),
                skip_if: None,
            },
        ],
    }
}

/// Test that we can connect to the flashblocks WebSocket stream.
async fn test_flashblocks_stream_connect(client: &TestClient) -> Result<()> {
    let stream = FlashblocksStream::connect(&client.flashblocks_ws_url).await?;

    tracing::debug!("Connected to flashblocks stream");

    stream.close().await?;

    Ok(())
}

/// Test that we can receive a flashblock message from the stream.
async fn test_flashblocks_stream_receive(client: &TestClient) -> Result<()> {
    let mut stream = FlashblocksStream::connect(&client.flashblocks_ws_url).await?;

    // Wait for a flashblock with timeout
    let flashblock = tokio::time::timeout(Duration::from_secs(5), stream.next_flashblock())
        .await
        .map_err(|_| eyre::eyre!("Timeout waiting for flashblock"))??;

    tracing::debug!(
        block_number = flashblock.metadata.block_number,
        index = flashblock.index,
        tx_count = flashblock.diff.transactions.len(),
        "Received flashblock"
    );

    // Verify basic structure
    ensure!(flashblock.metadata.block_number > 0, "Block number should be positive");

    stream.close().await?;

    Ok(())
}
