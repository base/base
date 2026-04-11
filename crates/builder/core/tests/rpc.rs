//! Integration tests for the Builder RPC extension.

use alloy_consensus::TxEip1559;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use alloy_rpc_client::RpcClient;
use base_alloy_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
use base_builder_core::BuilderApiExtension;
use base_execution_txpool::ValidatedTransaction;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::Account;

/// Sets up a test harness with the `BuilderApiExtension` installed.
async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
    let harness = TestHarness::builder().with_ext::<BuilderApiExtension>(()).build().await?;
    let client = harness.rpc_client()?;
    Ok((harness, client))
}

/// Creates a deposit transaction for testing.
fn create_deposit_tx() -> (Address, Bytes) {
    let sender = Account::Alice.address();
    let deposit_tx = TxDeposit {
        source_hash: Default::default(),
        from: sender,
        to: TxKind::Create,
        mint: 0,
        value: U256::ZERO,
        gas_limit: 21000,
        is_system_transaction: false,
        input: Default::default(),
    };
    let signed_tx: BaseTransactionSigned = deposit_tx.into();
    let encoded = signed_tx.encoded_2718();
    (sender, Bytes::from(encoded))
}

/// Creates an EIP-1559 transaction for testing.
fn create_eip1559_tx(chain_id: u64) -> (Address, Bytes) {
    let sender = Account::Bob.address();
    let tx = TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit: 21000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Default::default(),
    };
    let sig = Signature::new(U256::from(1), U256::from(2), false);
    let signed = BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Eip1559(tx), sig);
    let encoded = signed.encoded_2718();
    (sender, Bytes::from(encoded))
}

/// Verifies the RPC endpoint does not accept a deposit transaction.
/// The pool doesn't accept deposit transactions, but the RPC should decode it successfully.
#[tokio::test]
async fn test_insert_validated_deposit_tx() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let (sender, raw) = create_deposit_tx();
    let validated_tx = ValidatedTransaction {
        sender,
        raw,
        target_block_number: None,
        min_timestamp: None,
        max_timestamp: None,
    };

    let result: Result<(), _> =
        client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    // Pool rejects the tx (deposit type not supported in pool), but decode succeeded
    // Error code -32603 (InternalError) means decode worked, pool rejected
    let err = result.expect_err("expected pool rejection");
    let err_str = err.to_string();
    assert!(
        err_str.contains("-32603") || err_str.contains("pool rejected"),
        "expected InternalError from pool rejection, got: {err_str}"
    );
    Ok(())
}

/// Verifies the RPC endpoint accepts a valid EIP-1559 transaction.
/// The pool should accept this transaction type.
#[tokio::test]
async fn test_insert_validated_eip1559_tx() -> eyre::Result<()> {
    let (harness, client) = setup().await?;

    let (sender, raw) = create_eip1559_tx(harness.chain_id());
    let validated_tx = ValidatedTransaction {
        sender,
        raw,
        target_block_number: None,
        min_timestamp: None,
        max_timestamp: None,
    };

    // EIP-1559 transactions are supported by the pool
    let result: Result<(), _> =
        client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    assert!(result.is_ok(), "expected success, got: {:?}", result.unwrap_err());
    Ok(())
}

/// Verifies the RPC endpoint rejects an invalid transaction at the pool insertion stage.
#[tokio::test]
async fn test_insert_invalid_tx_fails() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    // Invalid raw bytes that can't be decoded (0xFF is not a valid tx type)
    let validated_tx = ValidatedTransaction {
        sender: Address::repeat_byte(0x01),
        raw: Bytes::from(vec![0xFF, 0x01, 0x02, 0x03]),
        target_block_number: None,
        min_timestamp: None,
        max_timestamp: None,
    };

    let result: Result<(), _> =
        client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    let err = result.expect_err("expected decode error");
    let err_str = err.to_string();
    assert!(
        err_str.contains("-32602") || err_str.contains("failed to decode"),
        "expected InvalidParams for decode failure, got: {err_str}"
    );
    Ok(())
}
