//! Tests for eth_call and eth_estimateGas.

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, U256};
use eyre::{Result, ensure};
use op_alloy_rpc_types::OpTransactionRequest;

use crate::{
    TestClient,
    harness::FlashblockHarness,
    tests::{Test, TestCategory, skip_if_no_addresses, skip_if_no_signer_or_recipient},
};

/// Build the call test category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "call".to_string(),
        description: Some("eth_call and eth_estimateGas tests".to_string()),
        tests: vec![
            Test {
                name: "eth_call_latest".to_string(),
                description: Some("eth_call against latest block".to_string()),
                run: Box::new(|client| Box::pin(test_eth_call_latest(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_addresses(client) })
                })),
            },
            Test {
                name: "eth_call_pending".to_string(),
                description: Some("eth_call against pending block".to_string()),
                run: Box::new(|client| Box::pin(test_eth_call_pending(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_addresses(client) })
                })),
            },
            Test {
                name: "eth_estimate_gas_latest".to_string(),
                description: Some("eth_estimateGas against latest block".to_string()),
                run: Box::new(|client| Box::pin(test_estimate_gas_latest(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_addresses(client) })
                })),
            },
            Test {
                name: "eth_estimate_gas_pending".to_string(),
                description: Some("eth_estimateGas against pending block".to_string()),
                run: Box::new(|client| Box::pin(test_estimate_gas_pending(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_addresses(client) })
                })),
            },
            Test {
                name: "flashblock_eth_call_sees_state".to_string(),
                description: Some(
                    "eth_call against pending sees state changes from flashblock tx".to_string(),
                ),
                run: Box::new(|client| Box::pin(test_flashblock_eth_call_sees_state(client))),
                skip_if: Some(Box::new(|client| {
                    Box::pin(async move { skip_if_no_signer_or_recipient(client) })
                })),
            },
        ],
    }
}

/// Get a pair of addresses for from/to in tests.
/// Uses signer as from (if available) and recipient as to (if available).
fn get_test_addresses(client: &TestClient) -> (Address, Address) {
    let from = client
        .signer_address()
        .or(client.recipient())
        .expect("at least one address should be configured");
    let to = client
        .recipient()
        .or(client.signer_address())
        .expect("at least one address should be configured");
    (from, to)
}

async fn test_eth_call_latest(client: &TestClient) -> Result<()> {
    let (from, to) = get_test_addresses(client);

    // Simple call that should succeed - just calling with no data
    let tx = OpTransactionRequest::default().from(from).to(to).value(U256::ZERO);

    let result = client.eth_call(&tx, BlockNumberOrTag::Latest).await?;
    tracing::debug!(?result, "eth_call result at latest");

    Ok(())
}

async fn test_eth_call_pending(client: &TestClient) -> Result<()> {
    let (from, to) = get_test_addresses(client);

    let tx = OpTransactionRequest::default().from(from).to(to).value(U256::ZERO);

    let result = client.eth_call(&tx, BlockNumberOrTag::Pending).await?;
    tracing::debug!(?result, "eth_call result at pending");

    Ok(())
}

async fn test_estimate_gas_latest(client: &TestClient) -> Result<()> {
    let (from, to) = get_test_addresses(client);

    let tx = OpTransactionRequest::default().from(from).to(to).value(U256::from(1000));

    let gas = client.estimate_gas(&tx, BlockNumberOrTag::Latest).await?;
    tracing::debug!(gas, "Estimated gas at latest");

    ensure!(gas >= 21000, "Gas estimate should be at least 21000 for transfer");
    Ok(())
}

async fn test_estimate_gas_pending(client: &TestClient) -> Result<()> {
    let (from, to) = get_test_addresses(client);

    let tx = OpTransactionRequest::default().from(from).to(to).value(U256::from(1000));

    let gas = client.estimate_gas(&tx, BlockNumberOrTag::Pending).await?;
    tracing::debug!(gas, "Estimated gas at pending");

    ensure!(gas >= 21000, "Gas estimate should be at least 21000 for transfer");
    Ok(())
}

/// Test that eth_call against pending block sees state changes from flashblock transactions.
///
/// This test:
/// 1. Waits for the start of a fresh block (flashblock index 0 or 1)
/// 2. Gets the recipient balance
/// 3. Sends ETH to the recipient
/// 4. Waits for tx in flashblock (must be same block)
/// 5. Verifies eth_call can now see the updated balance in pending state
async fn test_flashblock_eth_call_sees_state(client: &TestClient) -> Result<()> {
    // Verify we have a signer (required for sending transactions)
    client.signer_address().ok_or_else(|| eyre::eyre!("No signer"))?;

    let recipient = client.recipient().ok_or_else(|| eyre::eyre!("No recipient configured"))?;

    // Start the flashblock harness (waits for start of a fresh block)
    let mut harness = FlashblockHarness::new(client).await?;
    let block_number = harness.block_number();

    // Get recipient balance before
    let balance_before = client.get_balance(recipient, BlockNumberOrTag::Pending).await?;
    tracing::debug!(?balance_before, "Recipient balance before");

    // Send ETH to recipient
    let transfer_amount = U256::from(1u64); // 1 wei - minimum to detect state change
    let (tx_bytes, tx_hash) = client.create_transfer(recipient, transfer_amount, None).await?;

    tracing::info!(?tx_hash, block = block_number, "Sending ETH to recipient");
    client.send_raw_transaction(tx_bytes).await?;

    // Wait for tx in flashblock (fails if block boundary crossed)
    harness.wait_for_tx(tx_hash, Duration::from_secs(10)).await?;

    // Now verify eth_call sees the updated state in pending
    let balance_after = client.get_balance(recipient, BlockNumberOrTag::Pending).await?;
    tracing::debug!(?balance_after, "Recipient balance after flashblock tx");

    ensure!(
        balance_after > balance_before,
        "Recipient balance should increase: before={}, after={}",
        balance_before,
        balance_after
    );

    ensure!(
        balance_after >= balance_before + transfer_amount,
        "Recipient should have at least transfer amount more: before={}, after={}, transfer={}",
        balance_before,
        balance_after,
        transfer_amount
    );

    // Verify we're still in the same block (pending state test is valid)
    harness.assert_same_block(block_number)?;

    tracing::info!(
        block = block_number,
        flashblocks = harness.flashblock_count(),
        "eth_call state visibility verified in pending state within flashblock window"
    );

    harness.close().await?;
    Ok(())
}
