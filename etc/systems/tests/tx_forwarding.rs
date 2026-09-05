//! System tests for the transaction forwarding pipeline.
//!
//! These tests verify that transactions can be forwarded from mempool nodes
//! to builder nodes via the `base_insertValidatedTransaction` RPC endpoint, and
//! that validity transactions can also be submitted directly to the builder.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, TxReceipt};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, NoExtensions, ValidatedTransaction, ValidityOperator,
    ValidityPredicate,
};
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, SystemTestProviderExt,
    SystemTestStack, SystemTestStackBuilder,
};
use base_tx_forwarding::TxForwardingConfig;
use base_txpool_rpc::SendRawTransactionValidityOptions;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const COBALT_ACTIVATION_BLOCK: u64 = 0;
const DENIM_ACTIVATION_BLOCK: u64 = 1;
const ZENITH_ACTIVATION_BLOCK: u64 = 1;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const PENDING_TX_TIMEOUT: Duration = Duration::from_secs(15);

/// Waits until the builder knows a transaction, proving forwarding completed.
async fn wait_for_pending_transaction(provider: &RootProvider<Base>, tx_hash: B256) -> Result<()> {
    timeout(PENDING_TX_TIMEOUT, async {
        loop {
            if provider.get_transaction_by_hash(tx_hash).await?.is_some() {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("transaction did not become pending on the builder")?
}

/// Starts a separate mempool and builder pair using the native Denim payload builder with validity
/// transport enabled on both nodes.
async fn start_validity_system() -> Result<SystemTestStack> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_cobalt_activation_block(COBALT_ACTIVATION_BLOCK)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .with_base_zenith_activation_block(ZENITH_ACTIVATION_BLOCK)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100),
        )
        .with_experimental_validity_transactions()
        .with_payload_builder_cutover()
        .build()
        .await?;

    system.l2_builder_provider()?.wait_for_block(3, Duration::from_secs(15)).await?;
    system.l2_client_provider()?.wait_for_block(3, Duration::from_secs(15)).await?;

    Ok(system)
}

/// Creates a signed EIP-1559 transaction and returns the sender, raw bytes, and tx hash.
fn create_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    recipient: Address,
) -> Result<(Address, Bytes, alloy_primitives::B256)> {
    let sender = signer.address();

    let tx_request = BaseTransactionRequest::default()
        .from(sender)
        .to(recipient)
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(nonce);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("invalid transaction request: {e:?}"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let tx_hash = *signed_tx.hash();
    let raw_tx: Bytes = signed_tx.encoded_2718().into();

    Ok((sender, raw_tx, tx_hash))
}

/// Creates a signed, self-paying EIP-8130 transaction and returns its raw bytes and hash.
fn create_signed_eip8130_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce_sequence: u64,
) -> Result<(Bytes, B256)> {
    let tx = TxEip8130 {
        chain_id,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: vec![vec![Call { to: Address::repeat_byte(0xde), data: Bytes::new() }]],
        metadata: Bytes::new(),
        payer: None,
    };
    let signature = signer.sign_hash_sync(&tx.sender_signature_hash())?;
    let signed = Eip8130Signed::new(tx, signature.as_bytes().to_vec().into(), Bytes::new());
    let tx_hash = *signed.hash();
    Ok((signed.encoded_2718().into(), tx_hash))
}

/// Tests that a single transaction can be inserted via `base_insertValidatedTransaction`.
///
/// This is the foundational test for the forwarding pipeline. It verifies:
/// 1. The builder node has the `base_insertValidatedTransaction` RPC endpoint
/// 2. The endpoint accepts a valid pre-validated transaction
/// 3. The transaction is included in a block on the builder
#[tokio::test]
async fn test_insert_validated_transaction_single() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;

    // Wait for some blocks to be produced so the chain is ready
    timeout(Duration::from_secs(15), async {
        loop {
            let block = builder_provider.get_block_number().await?;
            if block >= 2 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Builder block production timed out")??;

    // Set up the signer with a funded account
    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Verify sender has balance
    let balance = builder_provider.get_balance(sender).await?;
    assert!(balance > U256::ZERO, "Sender should have balance");

    // Get current nonce
    let nonce = builder_provider.get_transaction_count(sender).await?;

    // Create a signed transaction
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (sender, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    // Create the ValidatedTransaction payload
    let validated_tx = ValidatedTransaction { sender, raw: raw_tx, extensions: NoExtensions {} };

    // Create RPC client for the builder
    let builder_rpc_url = system.l2_rpc_url()?;
    let rpc_client = RpcClient::builder().http(builder_rpc_url);

    // Call base_insertValidatedTransaction
    let result: Result<(), _> =
        rpc_client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    assert!(result.is_ok(), "base_insertValidatedTransaction should succeed, got: {result:?}");

    // Wait for the transaction to be included in a block
    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) =
                builder_provider.get_transaction_receipt(expected_tx_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out")?
    .wrap_err("Failed to get transaction receipt")?;

    // Verify the transaction was included
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    system.shutdown().await?;

    Ok(())
}

/// Full system test for the transaction forwarding pipeline.
///
/// This test verifies the complete flow:
/// 1. Client node receives a transaction
/// 2. `TxForwardingExtension` picks it up from the mempool
/// 3. Forwarder calls `base_insertValidatedTransaction` on the builder
/// 4. Transaction is included in a block on the builder
///
/// This is different from `test_insert_validated_transaction_single` which
/// directly calls the RPC endpoint. Here we test the full pipeline.
#[tokio::test]
async fn test_tx_forwarding_pipeline_system() -> Result<()> {
    // Build a system test stack with tx forwarding enabled on the client.
    // The client will forward transactions to the builder's RPC endpoint
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            // Empty vector here because the stack will populate it with the builder RPC URL on start
            TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100),
        )
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    // Wait for some blocks to be produced so both nodes are synced
    timeout(Duration::from_secs(15), async {
        loop {
            let builder_block = builder_provider.get_block_number().await?;
            let client_block = client_provider.get_block_number().await?;
            if builder_block >= 3 && client_block >= 3 {
                return Ok::<_, eyre::Error>((builder_block, client_block));
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Block production/sync timed out")??;

    // Set up the signer with a funded account
    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Wait for client to sync balance
    timeout(Duration::from_secs(15), async {
        loop {
            let client_balance = client_provider.get_balance(sender).await?;
            if client_balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for client to sync balance")??;

    // Get nonce from client (the node we'll send to)
    let nonce = client_provider.get_transaction_count(sender).await?;

    // Create a signed transaction
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    // Send the transaction to the CLIENT node (not builder)
    // The forwarding pipeline should forward it to the builder
    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction to client")?;
    let tx_hash = *pending_tx.tx_hash();
    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    // Wait for the transaction to be included in a block on the BUILDER
    // This proves the forwarding pipeline worked
    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) =
                builder_provider.get_transaction_receipt(expected_tx_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out on builder - forwarding may have failed")?
    .wrap_err("Failed to get transaction receipt")?;

    // Verify the transaction was included
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    system.shutdown().await?;

    Ok(())
}

/// Exercises every native-builder predicate kind through mempool ingress, forwarding, and builder
/// inclusion.
#[tokio::test]
async fn test_matching_validity_predicates_are_forwarded_and_included() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();
    client_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let recipient_balance_before = builder_provider.get_balance(recipient).await?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;
    let current_block = builder_provider.get_block_number().await?;
    let validity = vec![
        ValidityPredicate::Balance {
            address: sender,
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        },
        ValidityPredicate::Storage {
            address: recipient,
            slot: U256::from(1),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        },
        ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThan,
            value: U256::from(current_block),
        },
    ];
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);

    let tx_hash: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (raw_tx, SendRawTransactionValidityOptions { validity }),
        )
        .await?;

    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    let receipt = builder_provider.wait_for_receipt(expected_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));
    assert_eq!(
        builder_provider.get_balance(recipient).await?,
        recipient_balance_before + U256::from(1_000_000_000u64),
        "the validity metadata must not alter the signed transaction's state transition"
    );

    system.shutdown().await?;

    Ok(())
}

/// Verifies a validity transaction submitted directly to the builder is included without an
/// extra mempool-node hop.
#[tokio::test]
async fn test_validity_transaction_submitted_directly_to_builder_is_included() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;

    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_3.private_key)?;
    let sender = signer.address();
    builder_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let nonce = builder_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let recipient_balance_before = builder_provider.get_balance(recipient).await?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;
    let rpc_client = RpcClient::builder().http(system.l2_rpc_url()?);
    let tx_hash: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![ValidityPredicate::Balance {
                        address: sender,
                        op: ValidityOperator::GreaterThan,
                        value: U256::ZERO,
                    }],
                },
            ),
        )
        .await?;

    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    let receipt = builder_provider.wait_for_receipt(expected_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));
    assert_eq!(
        builder_provider.get_balance(recipient).await?,
        recipient_balance_before + U256::from(1_000_000_000u64),
        "direct builder ingress must not alter the signed transaction's state transition"
    );

    system.shutdown().await?;

    Ok(())
}

/// Verifies a Zenith EIP-8130 transaction can carry validity predicates through forwarding and be
/// included by the native Denim payload builder.
#[tokio::test]
async fn test_eip8130_validity_transaction_is_included_by_native_builder() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let sender = signer.address();
    client_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let nonce_sequence = client_provider.get_transaction_count(sender).await?;
    let (raw_tx, expected_tx_hash) =
        create_signed_eip8130_tx(&signer, L2_CHAIN_ID, nonce_sequence)?;
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);
    let tx_hash: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![ValidityPredicate::Balance {
                        address: sender,
                        op: ValidityOperator::GreaterThan,
                        value: U256::ZERO,
                    }],
                },
            ),
        )
        .await?;

    assert_eq!(tx_hash, expected_tx_hash);
    let receipt = builder_provider.wait_for_receipt(tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, tx_hash);
    assert!(receipt.inner.inner.status());

    system.shutdown().await?;

    Ok(())
}

/// Verifies a false state predicate parks a forwarded transaction until another transaction
/// changes the watched state and makes it eligible.
#[tokio::test]
async fn test_validity_transaction_lands_after_balance_predicate_becomes_true() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    let validity_signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let trigger_signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)?;
    client_provider.wait_for_balance(validity_signer.address(), Duration::from_secs(15)).await?;
    client_provider.wait_for_balance(trigger_signer.address(), Duration::from_secs(15)).await?;

    let watched: Address = "0x1000000000000000000000000000000000000042".parse()?;
    assert_eq!(builder_provider.get_balance(watched).await?, U256::ZERO);

    let validity_nonce = client_provider.get_transaction_count(validity_signer.address()).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_validity_tx, validity_tx_hash) =
        create_signed_eip1559_tx(&validity_signer, L2_CHAIN_ID, validity_nonce, recipient)?;
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);
    let submitted_hash: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_validity_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![ValidityPredicate::Balance {
                        address: watched,
                        op: ValidityOperator::GreaterThanOrEqual,
                        value: U256::from(1),
                    }],
                },
            ),
        )
        .await?;
    assert_eq!(submitted_hash, validity_tx_hash);

    // Seeing the transaction pending on the builder proves forwarding completed; advancing two
    // blocks without a receipt then proves the false predicate, rather than forwarding latency,
    // is what prevents inclusion.
    wait_for_pending_transaction(&builder_provider, validity_tx_hash).await?;
    let pending_at = builder_provider.get_block_number().await?;
    builder_provider.wait_for_block(pending_at + 2, Duration::from_secs(15)).await?;
    assert!(
        builder_provider.get_transaction_receipt(validity_tx_hash).await?.is_none(),
        "transaction landed while its balance predicate was false"
    );

    let trigger_nonce = client_provider.get_transaction_count(trigger_signer.address()).await?;
    let (_, raw_trigger_tx, trigger_tx_hash) =
        create_signed_eip1559_tx(&trigger_signer, L2_CHAIN_ID, trigger_nonce, watched)?;
    let pending_trigger = client_provider.send_raw_transaction(&raw_trigger_tx).await?;
    assert_eq!(*pending_trigger.tx_hash(), trigger_tx_hash);

    let trigger_receipt =
        builder_provider.wait_for_receipt(trigger_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    let validity_receipt =
        builder_provider.wait_for_receipt(validity_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert!(
        trigger_receipt.inner.block_number <= validity_receipt.inner.block_number,
        "validity transaction landed before the state change that satisfied it"
    );
    assert!(builder_provider.get_balance(watched).await? >= U256::from(1));

    system.shutdown().await?;

    Ok(())
}

/// Verifies future block predicates defer inclusion, terminal bounds expire, and recoverable
/// storage mismatches remain parked.
#[tokio::test]
async fn test_validity_block_predicates_defer_and_expire_transactions() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    let future_signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let expiring_signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)?;
    let storage_signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_3.private_key)?;
    client_provider.wait_for_balance(future_signer.address(), Duration::from_secs(15)).await?;
    client_provider.wait_for_balance(expiring_signer.address(), Duration::from_secs(15)).await?;
    client_provider.wait_for_balance(storage_signer.address(), Duration::from_secs(15)).await?;

    let current_block = builder_provider.get_block_number().await?;
    // Native Denim blocks advance every 200 ms, so leave enough time to submit and observe all
    // three transactions before the future predicate becomes true.
    let target_block = current_block + 50;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let future_nonce = client_provider.get_transaction_count(future_signer.address()).await?;
    let (_, raw_future_tx, future_tx_hash) =
        create_signed_eip1559_tx(&future_signer, L2_CHAIN_ID, future_nonce, recipient)?;
    let expiring_nonce = client_provider.get_transaction_count(expiring_signer.address()).await?;
    let (_, raw_expiring_tx, expiring_tx_hash) =
        create_signed_eip1559_tx(&expiring_signer, L2_CHAIN_ID, expiring_nonce, recipient)?;
    let storage_nonce = client_provider.get_transaction_count(storage_signer.address()).await?;
    let (_, raw_storage_tx, storage_tx_hash) =
        create_signed_eip1559_tx(&storage_signer, L2_CHAIN_ID, storage_nonce, recipient)?;
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);

    let submitted_future: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_future_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![ValidityPredicate::BlockNumber {
                        op: ValidityOperator::GreaterThanOrEqual,
                        value: U256::from(target_block),
                    }],
                },
            ),
        )
        .await?;
    assert_eq!(submitted_future, future_tx_hash);

    let submitted_expiring: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_expiring_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![
                        ValidityPredicate::BlockNumber {
                            op: ValidityOperator::GreaterThanOrEqual,
                            value: U256::from(target_block + 1),
                        },
                        ValidityPredicate::BlockNumber {
                            op: ValidityOperator::LessThanOrEqual,
                            value: U256::from(target_block),
                        },
                    ],
                },
            ),
        )
        .await?;
    assert_eq!(submitted_expiring, expiring_tx_hash);

    let submitted_storage: B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (
                raw_storage_tx,
                SendRawTransactionValidityOptions {
                    validity: vec![ValidityPredicate::Storage {
                        address: recipient,
                        slot: U256::from(1),
                        mask: U256::MAX,
                        op: ValidityOperator::Equal,
                        value: U256::from(2),
                    }],
                },
            ),
        )
        .await?;
    assert_eq!(submitted_storage, storage_tx_hash);

    wait_for_pending_transaction(&builder_provider, future_tx_hash).await?;
    wait_for_pending_transaction(&builder_provider, expiring_tx_hash).await?;
    wait_for_pending_transaction(&builder_provider, storage_tx_hash).await?;

    let future_receipt =
        builder_provider.wait_for_receipt(future_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert!(
        future_receipt.inner.block_number.is_some_and(|block| block >= target_block),
        "future-gated transaction landed before its target block"
    );

    builder_provider.wait_for_block(target_block + 2, Duration::from_secs(20)).await?;
    assert!(
        builder_provider.get_transaction_receipt(expiring_tx_hash).await?.is_none(),
        "transaction with contradictory block predicates was included"
    );
    timeout(Duration::from_secs(10), async {
        loop {
            if builder_provider.get_transaction_by_hash(expiring_tx_hash).await?.is_none() {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("expired validity transaction remained in the builder pool")??;
    assert!(
        builder_provider.get_transaction_receipt(storage_tx_hash).await?.is_none(),
        "transaction with a false storage predicate was included"
    );
    assert!(
        builder_provider.get_transaction_by_hash(storage_tx_hash).await?.is_some(),
        "recoverable storage predicate mismatch should remain pending"
    );

    system.shutdown().await?;

    Ok(())
}

/// Verifies malformed validity batches are rejected before the mempool can forward them.
#[tokio::test]
async fn test_invalid_validity_batches_are_rejected_at_mempool_ingress() -> Result<()> {
    let system = start_validity_system().await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    client_provider.wait_for_balance(signer.address(), Duration::from_secs(15)).await?;
    let nonce = client_provider.get_transaction_count(signer.address()).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_tx, tx_hash) = create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;
    let repeated_predicate = ValidityPredicate::Balance {
        address: signer.address(),
        op: ValidityOperator::GreaterThan,
        value: U256::ZERO,
    };
    let invalid_batches = vec![
        (Vec::new(), "validity predicates must not be empty"),
        (
            vec![repeated_predicate; DEFAULT_MAX_VALIDITY_PREDICATES + 1],
            "too many validity predicates",
        ),
        (
            vec![ValidityPredicate::Storage {
                address: recipient,
                slot: U256::ZERO,
                mask: U256::from(0xff),
                op: ValidityOperator::Equal,
                value: U256::from(0x100),
            }],
            "value bits set outside its mask",
        ),
    ];
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);

    for (validity, expected_error) in invalid_batches {
        let error = rpc_client
            .request::<_, B256>(
                "base_sendRawTransactionValidity",
                (raw_tx.clone(), SendRawTransactionValidityOptions { validity }),
            )
            .await
            .expect_err("invalid validity batch should be rejected");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected RPC error for invalid validity batch: {error}"
        );
    }

    assert!(client_provider.get_transaction_by_hash(tx_hash).await?.is_none());
    assert!(builder_provider.get_transaction_by_hash(tx_hash).await?.is_none());

    system.shutdown().await?;

    Ok(())
}

/// Tests that the forwarding pipeline handles high transaction load under rate limiting.
///
/// Uses all 4 available test accounts (`ANVIL_ACCOUNT_1` through `ANVIL_ACCOUNT_4`) to send
/// transactions concurrently, with `max_rps = 1` forcing the forwarder to buffer heavily.
/// Each account sends 10 transactions for a total of 40, verifying that all are eventually
/// forwarded to the builder and included in blocks despite the constrained send rate.
#[tokio::test]
async fn test_tx_forwarding_pipeline_system_high_load() -> Result<()> {
    const TXS_PER_ACCOUNT: usize = 10;

    let accounts = [&*ANVIL_ACCOUNT_1, &*ANVIL_ACCOUNT_2, &*ANVIL_ACCOUNT_3, &*ANVIL_ACCOUNT_4];

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_max_rps(1).with_resend_after_ms(30_000), // high resend window so we don't double-send
        )
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    // Wait for some blocks to be produced so both nodes are synced
    timeout(Duration::from_secs(15), async {
        loop {
            let builder_block = builder_provider.get_block_number().await?;
            let client_block = client_provider.get_block_number().await?;
            if builder_block >= 3 && client_block >= 3 {
                return Ok::<_, eyre::Error>((builder_block, client_block));
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Block production/sync timed out")??;

    // Set up signers for all accounts
    let signers: Vec<PrivateKeySigner> = accounts
        .iter()
        .map(|acct| {
            let hex = format!("0x{}", hex::encode(acct.private_key.as_slice()));
            hex.parse::<PrivateKeySigner>().map_err(|e| eyre::eyre!("invalid private key: {e:?}"))
        })
        .collect::<Result<Vec<PrivateKeySigner>>>()?;

    // Wait for all accounts to have balance on the client
    timeout(Duration::from_secs(15), async {
        loop {
            let mut all_funded = true;
            for signer in &signers {
                let balance = client_provider.get_balance(signer.address()).await?;
                if balance == U256::ZERO {
                    all_funded = false;
                    break;
                }
            }
            if all_funded {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for all accounts to sync balance")??;

    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;

    // Send TXS_PER_ACCOUNT transactions from each signer, interleaving accounts
    // to maximize concurrency pressure on the forwarder
    let mut expected: Vec<(Address, alloy_primitives::B256)> = Vec::new();
    let mut nonces: Vec<u64> = Vec::with_capacity(signers.len());
    for signer in &signers {
        let nonce = client_provider.get_transaction_count(signer.address()).await?;
        nonces.push(nonce);
    }

    for tx_idx in 0..TXS_PER_ACCOUNT {
        for (acct_idx, signer) in signers.iter().enumerate() {
            let nonce = nonces[acct_idx] + tx_idx as u64;
            let (_, raw_tx, expected_tx_hash) =
                create_signed_eip1559_tx(signer, L2_CHAIN_ID, nonce, recipient)?;

            let pending_tx = client_provider
                .send_raw_transaction(&raw_tx)
                .await
                .wrap_err_with(|| format!("Failed to send tx {tx_idx} from account {acct_idx}"))?;

            assert_eq!(
                *pending_tx.tx_hash(),
                expected_tx_hash,
                "Transaction hash mismatch for tx {tx_idx} from account {acct_idx}"
            );
            expected.push((signer.address(), expected_tx_hash));
        }
    }

    let total_txs = expected.len();

    // Wait for ALL transactions to be included on the builder.
    // With max_rps=1 and 40 txs, the forwarder needs significant time to drain.
    let mut received = vec![false; total_txs];

    timeout(Duration::from_secs(180), async {
        loop {
            let mut all_received = true;
            for (i, (sender, hash)) in expected.iter().enumerate() {
                if received[i] {
                    continue;
                }
                if let Some(receipt) = builder_provider.get_transaction_receipt(*hash).await? {
                    assert_eq!(receipt.inner.transaction_hash, *hash);
                    assert_eq!(receipt.inner.from, *sender);
                    assert_eq!(receipt.inner.to, Some(recipient));
                    received[i] = true;
                } else {
                    all_received = false;
                }
            }
            if all_received {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for all transactions - forwarding under load may have failed")??;

    let included_count = received.iter().filter(|&&r| r).count();
    assert_eq!(
        included_count, total_txs,
        "Expected all {total_txs} transactions to be included, got {included_count}"
    );

    system.shutdown().await?;

    Ok(())
}
