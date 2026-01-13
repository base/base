//! Tests for transaction receipts.

use std::time::Duration;

use alloy_primitives::{U256, b256};
use eyre::{Result, ensure};

use crate::{
    TestClient,
    harness::FlashblockHarness,
    tests::{Test, TestCategory, skip_if_no_signer_or_recipient},
};

/// Build the receipts test category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "receipts".to_string(),
        description: Some("Transaction receipt retrieval tests".to_string()),
        tests: vec![
            Test {
                name: "get_receipt_nonexistent".to_string(),
                description: Some("Get receipt for non-existent tx returns None".to_string()),
                run: Box::new(|client| Box::pin(test_receipt_nonexistent(client))),
                skip_if: None,
            },
            Test {
                name: "flashblock_receipt".to_string(),
                description: Some(
                    "Send tx and verify receipt available in pending state within same block"
                        .to_string(),
                ),
                run: Box::new(|client| Box::pin(test_flashblock_receipt(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_signer_or_recipient(client) })
                })),
            },
        ],
    }
}

async fn test_receipt_nonexistent(client: &TestClient) -> Result<()> {
    // Use a random hash that shouldn't exist
    let fake_hash = b256!("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef");

    let receipt = client.get_transaction_receipt(fake_hash).await?;
    ensure!(receipt.is_none(), "Receipt for non-existent tx should be None");

    Ok(())
}

/// Test that transaction receipts are available in pending state via flashblocks.
///
/// This test:
/// 1. Waits for the start of a fresh block (flashblock index 0 or 1)
/// 2. Sends a transaction
/// 3. Waits for it to appear in a flashblock (must be same block)
/// 4. Queries the receipt from pending state
/// 5. Verifies receipt structure
async fn test_flashblock_receipt(client: &TestClient) -> Result<()> {
    client.signer_address().ok_or_else(|| eyre::eyre!("No signer"))?;

    let recipient = client.recipient().ok_or_else(|| eyre::eyre!("No recipient configured"))?;

    // Start the flashblock harness (waits for start of a fresh block)
    let mut harness = FlashblockHarness::new(client).await?;
    let block_number = harness.block_number();

    // Send transaction
    let value = U256::from(1u64); // 1 wei - minimum to detect state change
    let (tx_bytes, tx_hash) = client.create_transfer(recipient, value, None).await?;

    tracing::info!(?tx_hash, block = block_number, "Sending transaction");
    client.send_raw_transaction(tx_bytes).await?;

    // Wait for tx to appear in flashblock (fails if block boundary crossed)
    harness.wait_for_tx(tx_hash, Duration::from_secs(10)).await?;

    // Query the receipt - should be available in pending state via flashblocks
    let receipt = client.get_transaction_receipt(tx_hash).await?;

    ensure!(
        receipt.is_some(),
        "Receipt should be available in pending state after tx appears in flashblock"
    );

    let receipt = receipt.unwrap();

    // Verify receipt structure
    ensure!(
        receipt.inner.transaction_hash == tx_hash,
        "Receipt tx hash mismatch: expected {}, got {}",
        tx_hash,
        receipt.inner.transaction_hash
    );

    ensure!(receipt.inner.inner.status(), "Transaction should have succeeded");

    tracing::debug!(
        tx_hash = ?receipt.inner.transaction_hash,
        block_number = ?receipt.inner.block_number,
        gas_used = ?receipt.inner.gas_used,
        "Got flashblock receipt"
    );

    // Verify we're still in the same block (pending state test is valid)
    harness.assert_same_block(block_number)?;

    tracing::info!(
        block = block_number,
        flashblocks = harness.flashblock_count(),
        "Receipt verified in pending state within flashblock window"
    );

    harness.close().await?;
    Ok(())
}
