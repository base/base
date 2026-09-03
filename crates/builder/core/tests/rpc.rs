//! Integration tests for the Builder RPC extension.

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, Signature, TxHash, TxKind, U256};
use alloy_rpc_client::RpcClient;
use alloy_signer::SignerSync;
use base_builder_core::{BuilderApiExtension, BuilderApiExtensionConfig};
use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, NoExtensions, TransactionValidity, ValidatedTransaction,
    ValidityOperator, ValidityPredicate,
};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::Account;
use base_txpool_rpc::{SendTransactionValidityExtension, SendTransactionValidityRequest};

/// Sets up a test harness with the `BuilderApiExtension` installed.
async fn setup(
    accept_validity: bool,
    max_validity_predicates: usize,
) -> eyre::Result<(TestHarness, RpcClient)> {
    let config = BuilderApiExtensionConfig::new(accept_validity, max_validity_predicates);
    let harness = TestHarness::builder().with_ext::<BuilderApiExtension>(config).build().await?;
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

/// Sets up a test harness with builder insertion and public validity ingress.
async fn setup_with_validity_ingress(
    accept_validity: bool,
    max_validity_predicates: usize,
) -> eyre::Result<(TestHarness, RpcClient)> {
    let config = BuilderApiExtensionConfig::new(accept_validity, max_validity_predicates);
    let mut builder = TestHarness::builder().with_ext::<BuilderApiExtension>(config);
    if accept_validity {
        builder = builder.with_ext::<SendTransactionValidityExtension>(max_validity_predicates);
    }
    let harness = builder.build().await?;
    let client = harness.rpc_client()?;
    Ok((harness, client))
}

/// Creates a recoverably signed EIP-1559 transaction for public validity ingress.
fn signed_eip1559_tx(chain_id: u64) -> Bytes {
    let account = Account::Alice;
    let request = BaseTransactionRequest::default()
        .from(account.address())
        .transaction_type(2u8)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .to(Address::ZERO)
        .with_nonce(0);
    let transaction = request.build_typed_tx().expect("valid transaction request");
    let signature = account
        .signer()
        .sign_hash_sync(&transaction.signature_hash())
        .expect("test account should sign transaction");

    transaction.into_signed(signature).encoded_2718().into()
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
    let (_harness, client) = setup(false, DEFAULT_MAX_VALIDITY_PREDICATES).await?;

    let (sender, raw) = create_deposit_tx();
    let validated_tx = ValidatedTransaction { sender, raw, extensions: NoExtensions {} };

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
    let (harness, client) = setup(false, DEFAULT_MAX_VALIDITY_PREDICATES).await?;

    let (sender, raw) = create_eip1559_tx(harness.chain_id());
    let validated_tx = ValidatedTransaction { sender, raw, extensions: NoExtensions {} };

    // EIP-1559 transactions are supported by the pool
    let result: Result<(), _> =
        client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    assert!(result.is_ok(), "expected success, got: {:?}", result.unwrap_err());
    Ok(())
}

/// Verifies the RPC endpoint rejects an invalid transaction at the pool insertion stage.
#[tokio::test]
async fn test_insert_invalid_tx_fails() -> eyre::Result<()> {
    let (_harness, client) = setup(false, DEFAULT_MAX_VALIDITY_PREDICATES).await?;

    // Invalid raw bytes that can't be decoded (0xFF is not a valid tx type)
    let validated_tx = ValidatedTransaction {
        sender: Address::repeat_byte(0x01),
        raw: Bytes::from(vec![0xFF, 0x01, 0x02, 0x03]),
        extensions: NoExtensions {},
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

/// Verifies validity-bearing requests require explicit builder opt-in.
#[tokio::test]
async fn test_validity_transactions_require_explicit_opt_in() -> eyre::Result<()> {
    let validity = TransactionValidity {
        validity: vec![ValidityPredicate::Balance {
            address: Account::Alice.address(),
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        }],
    };

    let (disabled_harness, disabled_client) = setup(false, DEFAULT_MAX_VALIDITY_PREDICATES).await?;
    let (sender, raw) = create_eip1559_tx(disabled_harness.chain_id());
    let disabled_tx = ValidatedTransaction { sender, raw, extensions: validity.clone() };
    let disabled: Result<(), _> =
        disabled_client.request("base_insertValidatedTransaction", (disabled_tx,)).await;
    assert!(
        disabled
            .expect_err("disabled builder should reject validity")
            .to_string()
            .contains("transaction extensions are disabled")
    );

    let (enabled_harness, enabled_client) = setup(true, DEFAULT_MAX_VALIDITY_PREDICATES).await?;
    let (sender, raw) = create_eip1559_tx(enabled_harness.chain_id());
    let enabled_tx = ValidatedTransaction { sender, raw, extensions: validity };
    let enabled: Result<(), _> =
        enabled_client.request("base_insertValidatedTransaction", (enabled_tx,)).await;
    assert!(enabled.is_ok(), "enabled builder should accept validity: {enabled:?}");

    Ok(())
}

/// Verifies the builder enforces its configured validity predicate limit.
#[tokio::test]
async fn test_validity_transactions_enforce_configured_limit() -> eyre::Result<()> {
    let (harness, client) = setup(true, 1).await?;
    let (sender, raw) = create_eip1559_tx(harness.chain_id());
    let predicate = ValidityPredicate::Balance {
        address: Account::Alice.address(),
        op: ValidityOperator::Equal,
        value: U256::ZERO,
    };
    let transaction = ValidatedTransaction {
        sender,
        raw,
        extensions: TransactionValidity { validity: vec![predicate; 2] },
    };

    let result: Result<(), _> =
        client.request("base_insertValidatedTransaction", (transaction,)).await;
    let error = result.expect_err("builder should reject validity above its configured limit");

    assert!(error.to_string().contains("too many validity predicates"));
    assert!(error.to_string().contains("maximum 1"));
    Ok(())
}

/// Verifies builders do not expose public validity ingress unless explicitly opted in.
#[tokio::test]
async fn test_send_transaction_validity_requires_explicit_opt_in() -> eyre::Result<()> {
    let (disabled_harness, disabled_client) = setup(false, DEFAULT_MAX_VALIDITY_PREDICATES).await?;
    let disabled: Result<TxHash, _> = disabled_client
        .request(
            "base_sendTransactionValidity",
            (SendTransactionValidityRequest {
                tx: signed_eip1559_tx(disabled_harness.chain_id()),
                validity: Vec::new(),
            },),
        )
        .await;
    let disabled_error = disabled
        .expect_err("disabled builder should not expose base_sendTransactionValidity")
        .to_string();
    assert!(
        disabled_error.contains("-32601") || disabled_error.to_ascii_lowercase().contains("method"),
        "expected method-not-found for disabled builder, got: {disabled_error}"
    );

    let (enabled_harness, enabled_client) =
        setup_with_validity_ingress(true, DEFAULT_MAX_VALIDITY_PREDICATES).await?;
    let enabled: Result<TxHash, _> = enabled_client
        .request(
            "base_sendTransactionValidity",
            (SendTransactionValidityRequest {
                tx: signed_eip1559_tx(enabled_harness.chain_id()),
                validity: vec![ValidityPredicate::Balance {
                    address: Account::Alice.address(),
                    op: ValidityOperator::Equal,
                    value: U256::ZERO,
                }],
            },),
        )
        .await;
    assert!(enabled.is_ok(), "enabled builder should accept validity ingress: {enabled:?}");

    Ok(())
}

/// Verifies public validity ingress enforces the builder's configured predicate limit.
#[tokio::test]
async fn test_send_transaction_validity_enforces_configured_limit() -> eyre::Result<()> {
    let (harness, client) = setup_with_validity_ingress(true, 1).await?;
    let predicate = ValidityPredicate::Balance {
        address: Account::Alice.address(),
        op: ValidityOperator::Equal,
        value: U256::ZERO,
    };
    let result: Result<TxHash, _> = client
        .request(
            "base_sendTransactionValidity",
            (SendTransactionValidityRequest {
                tx: signed_eip1559_tx(harness.chain_id()),
                validity: vec![predicate; 2],
            },),
        )
        .await;
    let error = result.expect_err("builder should reject validity above its configured limit");

    assert!(error.to_string().contains("too many validity predicates"));
    assert!(error.to_string().contains("maximum 1"));
    Ok(())
}
