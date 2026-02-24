//! Integration tests for the `base_insertValidatedTransactions` RPC endpoint.
//!
//! Uses `base_client_node::test_utils::TestHarness` to spin up a test node
//! with the `ValidatedTxRpcExtension` enabled.

use alloy_primitives::{Address, U256};
use alloy_rpc_client::RpcClient;
use base_client_node::test_utils::TestHarness;
use base_primitives::{TransactionSignature, ValidatedTransaction};
use base_validated_tx_rpc::{InsertResult, ValidatedTxRpcConfig, ValidatedTxRpcExtension};
use eyre::Result;

/// Helper to create a valid EIP-1559 `ValidatedTransaction` for testing.
fn create_test_validated_tx(nonce: u64, from: Address, chain_id: u64) -> ValidatedTransaction {
    ValidatedTransaction {
        from,
        balance: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        state_nonce: nonce,
        bytecode_hash: None,
        tx_type: 2, // EIP-1559
        chain_id: Some(chain_id),
        nonce,
        to: Some(Address::repeat_byte(0x42)),
        value: U256::from(1000),
        gas_limit: 21000,
        gas_price: None,
        max_fee_per_gas: Some(1_000_000_000),
        max_priority_fee_per_gas: Some(100_000_000),
        input: alloy_primitives::Bytes::default(),
        access_list: None,
        signature: TransactionSignature { v: 0, r: U256::from(1), s: U256::from(2) },
    }
}

async fn setup() -> Result<(TestHarness, RpcClient, u64)> {
    let harness = TestHarness::builder()
        .with_ext::<ValidatedTxRpcExtension>(ValidatedTxRpcConfig { enabled: true })
        .build()
        .await?;
    let client = harness.rpc_client()?;
    let chain_id = harness.chain_id();
    Ok((harness, client, chain_id))
}

#[tokio::test]
async fn test_insert_single_validated_transaction() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let tx = create_test_validated_tx(0, Address::repeat_byte(0x01), chain_id);
    let results: Vec<InsertResult> =
        client.request("base_insertValidatedTransactions", (vec![tx],)).await?;

    assert_eq!(results.len(), 1, "should return exactly one result");
    let result = &results[0];
    assert!(result.success, "transaction insertion should succeed: {:?}", result.error);
    assert_ne!(
        result.tx_hash,
        alloy_primitives::B256::ZERO,
        "successful insertion should have non-zero tx hash"
    );
    assert!(result.error.is_none(), "successful insertion should not have error");

    Ok(())
}

#[tokio::test]
async fn test_insert_multiple_validated_transactions() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let from = Address::repeat_byte(0x02);
    let txs: Vec<ValidatedTransaction> =
        (0..3).map(|nonce| create_test_validated_tx(nonce, from, chain_id)).collect();

    let results: Vec<InsertResult> =
        client.request("base_insertValidatedTransactions", (txs,)).await?;

    assert_eq!(results.len(), 3, "should return results for all 3 transactions");
    for (i, result) in results.iter().enumerate() {
        assert!(result.success, "tx {} insertion should succeed: {:?}", i, result.error);
        assert_ne!(
            result.tx_hash,
            alloy_primitives::B256::ZERO,
            "tx {i} should have non-zero hash"
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_insert_validated_transaction_with_invalid_type() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let mut tx = create_test_validated_tx(0, Address::repeat_byte(0x03), chain_id);
    tx.tx_type = 99; // Unsupported type

    let result: Result<Vec<InsertResult>, _> =
        client.request("base_insertValidatedTransactions", (vec![tx],)).await;

    // Should return an error since all transactions failed
    assert!(result.is_err(), "unsupported tx type should cause RPC error");

    Ok(())
}

#[tokio::test]
async fn test_insert_mixed_valid_invalid_transactions() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let valid_tx = create_test_validated_tx(0, Address::repeat_byte(0x04), chain_id);
    let mut invalid_tx = create_test_validated_tx(0, Address::repeat_byte(0x05), chain_id);
    invalid_tx.tx_type = 99; // Make it invalid

    let results: Vec<InsertResult> =
        client.request("base_insertValidatedTransactions", (vec![valid_tx, invalid_tx],)).await?;

    assert_eq!(results.len(), 2, "should return results for both transactions");

    // First tx should succeed
    assert!(results[0].success, "valid tx should succeed");
    assert_ne!(
        results[0].tx_hash,
        alloy_primitives::B256::ZERO,
        "valid tx should have non-zero hash"
    );

    // Second tx should fail
    assert!(!results[1].success, "invalid tx should fail");
    assert!(results[1].error.is_some(), "invalid tx should have error message");

    Ok(())
}

#[tokio::test]
async fn test_insert_legacy_transaction() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let tx = ValidatedTransaction {
        from: Address::repeat_byte(0x06),
        balance: U256::from(1_000_000_000_000_000_000u128),
        state_nonce: 0,
        bytecode_hash: None,
        tx_type: 0, // Legacy
        chain_id: Some(chain_id),
        nonce: 0,
        to: Some(Address::repeat_byte(0x42)),
        value: U256::from(1000),
        gas_limit: 21000,
        gas_price: Some(1_000_000_000),
        max_fee_per_gas: None,
        max_priority_fee_per_gas: None,
        input: alloy_primitives::Bytes::default(),
        access_list: None,
        signature: TransactionSignature { v: 27, r: U256::from(1), s: U256::from(2) },
    };

    let results: Vec<InsertResult> =
        client.request("base_insertValidatedTransactions", (vec![tx],)).await?;

    assert_eq!(results.len(), 1, "should return exactly one result");
    assert!(results[0].success, "legacy tx insertion should succeed: {:?}", results[0].error);

    Ok(())
}

#[tokio::test]
async fn test_insert_eip2930_transaction() -> Result<()> {
    let (_harness, client, chain_id) = setup().await?;

    let tx = ValidatedTransaction {
        from: Address::repeat_byte(0x07),
        balance: U256::from(1_000_000_000_000_000_000u128),
        state_nonce: 0,
        bytecode_hash: None,
        tx_type: 1, // EIP-2930
        chain_id: Some(chain_id),
        nonce: 0,
        to: Some(Address::repeat_byte(0x42)),
        value: U256::from(1000),
        gas_limit: 21000,
        gas_price: Some(1_000_000_000),
        max_fee_per_gas: None,
        max_priority_fee_per_gas: None,
        input: alloy_primitives::Bytes::default(),
        access_list: Some(Default::default()),
        signature: TransactionSignature { v: 0, r: U256::from(1), s: U256::from(2) },
    };

    let results: Vec<InsertResult> =
        client.request("base_insertValidatedTransactions", (vec![tx],)).await?;

    assert_eq!(results.len(), 1, "should return exactly one result");
    assert!(results[0].success, "EIP-2930 tx insertion should succeed: {:?}", results[0].error);

    Ok(())
}

#[tokio::test]
async fn test_insert_empty_transactions_list() -> Result<()> {
    let (_harness, client, _chain_id) = setup().await?;

    let results: Vec<InsertResult> = client
        .request("base_insertValidatedTransactions", (Vec::<ValidatedTransaction>::new(),))
        .await?;

    assert!(results.is_empty(), "empty input should return empty results");

    Ok(())
}

#[tokio::test]
async fn test_extension_disabled_returns_method_not_found() -> Result<()> {
    // Setup with extension disabled
    let harness = TestHarness::builder()
        .with_ext::<ValidatedTxRpcExtension>(ValidatedTxRpcConfig { enabled: false })
        .build()
        .await?;
    let client = harness.rpc_client()?;

    let tx = create_test_validated_tx(0, Address::repeat_byte(0x08), harness.chain_id());
    let result: Result<Vec<InsertResult>, _> =
        client.request("base_insertValidatedTransactions", (vec![tx],)).await;

    assert!(result.is_err(), "should fail when extension is disabled");
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("not found") || err_str.contains("Method not found"),
        "error should indicate method not found: {err_str}"
    );

    Ok(())
}
