//! Tests for contract interactions via flashblocks.

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use eyre::{Result, ensure};
use op_alloy_rpc_types::OpTransactionRequest;

use crate::{
    TestClient,
    harness::FlashblockHarness,
    tests::{Test, TestCategory, skip_if_no_signer},
};

// Define the Counter contract interface using sol! macro for ABI encoding.
// The bytecode is compiled separately with forge (solc 0.8.30).
//
// Source (src/Counter.sol):
// ```solidity
// // SPDX-License-Identifier: UNLICENSED
// pragma solidity ^0.8.20;
// contract Counter {
//     uint256 public count;
//     function increment() external { count++; }
//     function getCount() external view returns (uint256) { return count; }
// }
// ```
alloy_sol_macro::sol! {
    /// Increment the counter.
    function increment() external;
    /// Get the current count.
    function getCount() external view returns (uint256);
}

// Counter contract bytecode compiled with forge (solc 0.8.30)
// count is stored in slot 0, anyone can increment
const COUNTER_BYTECODE: &str = "6080604052348015600e575f5ffd5b506101898061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061003f575f3560e01c806306661abd14610043578063a87d942c14610061578063d09de08a1461007f575b5f5ffd5b61004b610089565b60405161005891906100c6565b60405180910390f35b61006961008e565b60405161007691906100c6565b60405180910390f35b610087610096565b005b5f5481565b5f5f54905090565b5f5f8154809291906100a79061010c565b9190505550565b5f819050919050565b6100c0816100ae565b82525050565b5f6020820190506100d95f8301846100b7565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f610116826100ae565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8203610148576101476100df565b5b60018201905091905056fea2646970667358221220f20d10175682bbbd1b6bb8f4176629c4124ad6a6532bcaf2cfaa2ed6771b941a64736f6c634300081e0033";

/// Build the contracts test category.
pub(crate) fn category() -> TestCategory {
    TestCategory {
        name: "contracts".to_string(),
        description: Some("Contract deployment and interaction tests".to_string()),
        tests: vec![Test {
            name: "flashblock_counter_increment".to_string(),
            description: Some(
                "Deploy Counter, increment, and verify state change in flashblock".to_string(),
            ),
            run: Box::new(|client| Box::pin(test_flashblock_counter_increment(client))),
            skip_if: Some(Box::new(|client| Box::pin(async move { skip_if_no_signer(client) }))),
        }],
    }
}

/// Test that deploys a Counter contract and verifies increment is visible via flashblocks.
///
/// This test:
/// 1. Deploys a Counter contract (only owner can increment)
/// 2. Waits for deployment in flashblock
/// 3. Starts fresh flashblock window
/// 4. Queries pre-state (count = 0)
/// 5. Sends increment transaction
/// 6. Waits for tx in flashblock
/// 7. Queries post-state (count = 1)
/// 8. Verifies still same block
async fn test_flashblock_counter_increment(client: &TestClient) -> Result<()> {
    // Verify we have a signer
    client.signer_address().ok_or_else(|| eyre::eyre!("No signer"))?;

    // Deploy the Counter contract
    let contract_address = deploy_counter(client).await?;
    tracing::info!(?contract_address, "Counter contract deployed");

    // Start a fresh flashblock window for the increment test
    let mut harness = FlashblockHarness::new(client).await?;
    let block_number = harness.block_number();

    // Query pre-state: count should be 0
    let count_before = call_get_count(client, contract_address).await?;
    tracing::debug!(count = %count_before, block = block_number, "Count before increment");

    ensure!(count_before == U256::ZERO, "Initial count should be 0, got {}", count_before);

    // Send increment transaction
    let (tx_bytes, tx_hash) = create_increment_tx(client, contract_address).await?;

    tracing::info!(?tx_hash, block = block_number, "Sending increment transaction");
    client.send_raw_transaction(tx_bytes).await?;

    // Wait for tx in flashblock (fails if block boundary crossed)
    harness.wait_for_tx(tx_hash, Duration::from_secs(10)).await?;

    // Query post-state: count should be 1 in pending state
    let count_after = call_get_count(client, contract_address).await?;
    tracing::debug!(count = %count_after, "Count after flashblock tx");

    ensure!(count_after == U256::from(1), "Count should be 1 after increment, got {}", count_after);

    // Verify we're still in the same block (pending state test is valid)
    harness.assert_same_block(block_number)?;

    tracing::info!(
        block = block_number,
        flashblocks = harness.flashblock_count(),
        "Counter increment verified in pending state within flashblock window"
    );

    harness.close().await?;
    Ok(())
}

/// Deploy the Counter contract and return its address.
async fn deploy_counter(client: &TestClient) -> Result<Address> {
    let owner = client.signer_address().ok_or_else(|| eyre::eyre!("No signer"))?;

    // Get the contract bytecode
    let bytecode = hex::decode(COUNTER_BYTECODE)
        .map_err(|e| eyre::eyre!("Failed to decode bytecode: {}", e))?;

    // Get nonce
    let nonce = client.get_next_nonce().await?;

    // Build deployment transaction using with_deploy_code for contract creation
    let mut tx_request = OpTransactionRequest::default()
        .from(owner)
        .with_deploy_code(Bytes::from(bytecode))
        .nonce(nonce)
        .gas_limit(500_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000);
    tx_request.set_chain_id(client.chain_id());

    // Sign and send
    let (tx_bytes, tx_hash) = client.sign_transaction(tx_request)?;
    tracing::debug!(?tx_hash, "Deploying Counter contract");

    // Use sync mode to wait for deployment
    let receipt = client.send_raw_transaction_sync(tx_bytes, Some(6_000)).await?;

    ensure!(receipt.inner.inner.status(), "Contract deployment failed");

    let contract_address = receipt
        .inner
        .contract_address
        .ok_or_else(|| eyre::eyre!("No contract address in receipt"))?;

    Ok(contract_address)
}

/// Call getCount() on the Counter contract.
async fn call_get_count(client: &TestClient, contract: Address) -> Result<U256> {
    let call_data = getCountCall {}.abi_encode();

    let tx = OpTransactionRequest::default().to(contract).input(Bytes::from(call_data).into());

    let result = client.eth_call(&tx, BlockNumberOrTag::Pending).await?;

    // Decode the result - returns U256 directly
    let count = getCountCall::abi_decode_returns(&result)
        .map_err(|e| eyre::eyre!("Failed to decode getCount result: {}", e))?;

    Ok(count)
}

/// Create an increment() transaction.
async fn create_increment_tx(client: &TestClient, contract: Address) -> Result<(Bytes, B256)> {
    let owner = client.signer_address().ok_or_else(|| eyre::eyre!("No signer"))?;

    let call_data = incrementCall {}.abi_encode();

    // Get nonce
    let nonce = client.get_next_nonce().await?;

    let mut tx_request = OpTransactionRequest::default()
        .from(owner)
        .to(contract)
        .input(Bytes::from(call_data).into())
        .nonce(nonce)
        .gas_limit(100_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000);
    tx_request.set_chain_id(client.chain_id());

    client.sign_transaction(tx_request)
}
