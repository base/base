//! Integration tests covering the Metering RPC surface area.

use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, U256, address, bytes};
use alloy_rpc_client::RpcClient;
use base_bundles::{Bundle, MeterBundleResponse};
use base_metering::MeteringExtension;
use base_test_utils::{Account, TestHarness};
use op_alloy_consensus::OpTxEnvelope;
use reth_optimism_primitives::OpTransactionSigned;
use reth_transaction_pool::test_utils::TransactionBuilder;

/// Helper function to create a Bundle with default fields.
fn create_bundle(txs: Vec<Bytes>, block_number: u64, min_timestamp: Option<u64>) -> Bundle {
    Bundle {
        txs,
        block_number,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    }
}

/// Set up a test harness with the metering extension and return an RPC client.
async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
    let harness =
        TestHarness::builder().with_extension(MeteringExtension::new(true)).build().await?;

    let client = harness.rpc_client()?;
    Ok((harness, client))
}

#[tokio::test]
async fn test_meter_bundle_empty() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let bundle = create_bundle(vec![], 0, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    assert_eq!(response.results.len(), 0);
    assert_eq!(response.total_gas_used, 0);
    assert_eq!(response.gas_fees, U256::from(0));
    assert_eq!(response.state_block_number, 0);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_single_transaction() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let sender_address = Account::Alice.address();
    let sender_secret = Account::Alice.signer_b256();

    // Build a transaction
    let tx = TransactionBuilder::default()
        .signer(sender_secret)
        .chain_id(84532)
        .nonce(0)
        .to(address!("0x1111111111111111111111111111111111111111"))
        .value(1000)
        .gas_limit(21_000)
        .max_fee_per_gas(1_000_000_000) // 1 gwei
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559();

    let signed_tx =
        OpTransactionSigned::Eip1559(tx.as_eip1559().expect("eip1559 transaction").clone());
    let envelope: OpTxEnvelope = signed_tx.into();

    // Encode transaction
    let tx_bytes = Bytes::from(envelope.encoded_2718());

    let bundle = create_bundle(vec![tx_bytes], 0, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.total_gas_used, 21_000);
    assert!(response.total_execution_time_us > 0);

    let result = &response.results[0];
    assert_eq!(result.from_address, sender_address);
    assert_eq!(result.to_address, Some(address!("0x1111111111111111111111111111111111111111")));
    assert_eq!(result.gas_used, 21_000);
    assert_eq!(result.gas_price, 1_000_000_000);
    assert!(result.execution_time_us > 0);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_multiple_transactions() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let address1 = Account::Alice.address();
    let secret1 = Account::Alice.signer_b256();

    let tx1_inner = TransactionBuilder::default()
        .signer(secret1)
        .chain_id(84532)
        .nonce(0)
        .to(address!("0x1111111111111111111111111111111111111111"))
        .value(1000)
        .gas_limit(21_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559();

    let tx1_signed =
        OpTransactionSigned::Eip1559(tx1_inner.as_eip1559().expect("eip1559 transaction").clone());
    let tx1_envelope: OpTxEnvelope = tx1_signed.into();
    let tx1_bytes = Bytes::from(tx1_envelope.encoded_2718());

    // Second transaction from second account
    let address2 = Account::Bob.address();
    let secret2 = Account::Bob.signer_b256();

    let tx2_inner = TransactionBuilder::default()
        .signer(secret2)
        .chain_id(84532)
        .nonce(0)
        .to(address!("0x2222222222222222222222222222222222222222"))
        .value(2000)
        .gas_limit(21_000)
        .max_fee_per_gas(2_000_000_000)
        .max_priority_fee_per_gas(2_000_000_000)
        .into_eip1559();

    let tx2_signed =
        OpTransactionSigned::Eip1559(tx2_inner.as_eip1559().expect("eip1559 transaction").clone());
    let tx2_envelope: OpTxEnvelope = tx2_signed.into();
    let tx2_bytes = Bytes::from(tx2_envelope.encoded_2718());

    let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    assert_eq!(response.results.len(), 2);
    assert_eq!(response.total_gas_used, 42_000);
    assert!(response.total_execution_time_us > 0);

    // Check first transaction
    let result1 = &response.results[0];
    assert_eq!(result1.from_address, address1);
    assert_eq!(result1.gas_used, 21_000);
    assert_eq!(result1.gas_price, 1_000_000_000);

    // Check second transaction
    let result2 = &response.results[1];
    assert_eq!(result2.from_address, address2);
    assert_eq!(result2.gas_used, 21_000);
    assert_eq!(result2.gas_price, 2_000_000_000);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_invalid_transaction() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let bundle = create_bundle(
        vec![bytes!("0xdeadbeef")], // Invalid transaction data
        0,
        None,
    );

    let result: Result<MeterBundleResponse, _> =
        client.request("base_meterBundle", (bundle,)).await;

    assert!(result.is_err());

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_uses_latest_block() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    // Metering always uses the latest block state, regardless of bundle.block_number
    let bundle = create_bundle(vec![], 0, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    // Should return the latest block number (genesis block 0)
    assert_eq!(response.state_block_number, 0);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_ignores_bundle_block_number() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    // Even if bundle.block_number is different, it should use the latest block
    // In this test, we specify block_number=0 in the bundle
    let bundle1 = create_bundle(vec![], 0, None);
    let response1: MeterBundleResponse = client.request("base_meterBundle", (bundle1,)).await?;

    // Try with a different bundle.block_number (999 - arbitrary value)
    // Since we can't create future blocks, we use a different value to show it's ignored
    let bundle2 = create_bundle(vec![], 999, None);
    let response2: MeterBundleResponse = client.request("base_meterBundle", (bundle2,)).await?;

    // Both should return the same state_block_number (the latest block)
    // because the implementation always uses Latest, not bundle.block_number
    assert_eq!(response1.state_block_number, response2.state_block_number);
    assert_eq!(response1.state_block_number, 0); // Genesis block

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_custom_timestamp() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    // Test that bundle.min_timestamp is used for simulation.
    // The timestamp affects block.timestamp in the EVM during simulation but is not
    // returned in the response.
    let custom_timestamp = 1234567890;
    let bundle = create_bundle(vec![], 0, Some(custom_timestamp));

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    // Verify the request succeeded with custom timestamp
    assert_eq!(response.results.len(), 0);
    assert_eq!(response.total_gas_used, 0);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_arbitrary_block_number() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    // Since we now ignore bundle.block_number and always use the latest block,
    // any block_number value should work (it's only used for bundle validity in TIPS)
    let bundle = create_bundle(vec![], 999999, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    // Should succeed and use the latest block (genesis block 0)
    assert_eq!(response.state_block_number, 0);

    Ok(())
}

#[tokio::test]
async fn test_meter_bundle_gas_calculations() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let secret1 = Account::Alice.signer_b256();
    let secret2 = Account::Bob.signer_b256();

    // First transaction with 3 gwei gas price
    let tx1_inner = TransactionBuilder::default()
        .signer(secret1)
        .chain_id(84532)
        .nonce(0)
        .to(address!("0x1111111111111111111111111111111111111111"))
        .value(1000)
        .gas_limit(21_000)
        .max_fee_per_gas(3_000_000_000) // 3 gwei
        .max_priority_fee_per_gas(3_000_000_000)
        .into_eip1559();

    let signed_tx1 =
        OpTransactionSigned::Eip1559(tx1_inner.as_eip1559().expect("eip1559 transaction").clone());
    let envelope1: OpTxEnvelope = signed_tx1.into();
    let tx1_bytes = Bytes::from(envelope1.encoded_2718());

    // Second transaction with 7 gwei gas price
    let tx2_inner = TransactionBuilder::default()
        .signer(secret2)
        .chain_id(84532)
        .nonce(0)
        .to(address!("0x2222222222222222222222222222222222222222"))
        .value(2000)
        .gas_limit(21_000)
        .max_fee_per_gas(7_000_000_000) // 7 gwei
        .max_priority_fee_per_gas(7_000_000_000)
        .into_eip1559();

    let signed_tx2 =
        OpTransactionSigned::Eip1559(tx2_inner.as_eip1559().expect("eip1559 transaction").clone());
    let envelope2: OpTxEnvelope = signed_tx2.into();
    let tx2_bytes = Bytes::from(envelope2.encoded_2718());

    let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

    let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

    assert_eq!(response.results.len(), 2);

    // Check first transaction (3 gwei)
    let result1 = &response.results[0];
    let expected_gas_fees_1 = U256::from(21_000) * U256::from(3_000_000_000u64);
    assert_eq!(result1.gas_fees, expected_gas_fees_1);
    assert_eq!(result1.gas_price, U256::from(3000000000u64));
    assert_eq!(result1.coinbase_diff, expected_gas_fees_1);

    // Check second transaction (7 gwei)
    let result2 = &response.results[1];
    let expected_gas_fees_2 = U256::from(21_000) * U256::from(7_000_000_000u64);
    assert_eq!(result2.gas_fees, expected_gas_fees_2);
    assert_eq!(result2.gas_price, U256::from(7000000000u64));
    assert_eq!(result2.coinbase_diff, expected_gas_fees_2);

    // Check bundle totals
    let total_gas_fees = expected_gas_fees_1 + expected_gas_fees_2;
    assert_eq!(response.gas_fees, total_gas_fees);
    assert_eq!(response.coinbase_diff, total_gas_fees);
    assert_eq!(response.total_gas_used, 42_000);

    // Bundle gas price should be weighted average: (3*21000 + 7*21000) / (21000 + 21000) = 5 gwei
    assert_eq!(response.bundle_gas_price, U256::from(5000000000u64));

    Ok(())
}
