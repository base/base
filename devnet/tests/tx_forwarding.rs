//! End-to-end tests for the transaction forwarding pipeline.
//!
//! These tests verify that transactions can be forwarded from mempool nodes
//! to builder nodes via the `base_insertValidatedTransaction` RPC endpoint.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_client::RpcClient;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_tx_forwarding::TxForwardingConfig;
use base_txpool::ValidatedTransaction;
use devnet::{
    DevnetBuilder,
    config::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4},
};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Creates a signed EIP-1559 transaction and returns the sender, raw bytes, and tx hash.
fn create_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    recipient: Address,
) -> Result<(Address, Bytes, alloy_primitives::B256)> {
    use base_alloy_network::TransactionBuilder;
    use base_alloy_rpc_types::OpTransactionRequest;

    let sender = signer.address();

    let tx_request = OpTransactionRequest::default()
        .from(sender)
        .to(recipient)
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(nonce);

    let tx = tx_request.build_typed_tx().map_err(|_| eyre::eyre!("invalid transaction request"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let tx_hash = *signed_tx.hash();
    let raw_tx: Bytes = signed_tx.encoded_2718().into();

    Ok((sender, raw_tx, tx_hash))
}

/// Tests that a single transaction can be inserted via `base_insertValidatedTransaction`.
///
/// This is the foundational test for the forwarding pipeline. It verifies:
/// 1. The builder node has the `base_insertValidatedTransaction` RPC endpoint
/// 2. The endpoint accepts a valid pre-validated transaction
/// 3. The transaction is included in a block on the builder
#[tokio::test]
async fn test_insert_validated_transaction_single() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;

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
    let validated_tx = ValidatedTransaction { sender, raw: raw_tx };

    // Create RPC client for the builder
    let builder_rpc_url = devnet.l2_rpc_url()?;
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

    Ok(())
}

/// Tests that invalid transaction bytes are rejected with appropriate error.
#[tokio::test]
async fn test_insert_validated_transaction_invalid_bytes() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    // Wait for builder to be ready
    let builder_provider = devnet.l2_builder_provider()?;
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

    // Create RPC client for the builder
    let builder_rpc_url = devnet.l2_rpc_url()?;
    let rpc_client = RpcClient::builder().http(builder_rpc_url);

    // Create an invalid ValidatedTransaction with garbage bytes
    let validated_tx = ValidatedTransaction {
        sender: Address::repeat_byte(0x42),
        raw: Bytes::from(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    };

    // Call base_insertValidatedTransaction - should fail
    let result: Result<(), _> =
        rpc_client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    let err = result.expect_err("expected decode error for invalid bytes");
    let err_str = err.to_string();

    // Should get InvalidParams error (-32602) for decode failure
    assert!(
        err_str.contains("-32602") || err_str.contains("failed to decode"),
        "expected InvalidParams for decode failure, got: {err_str}"
    );

    Ok(())
}

/// Full end-to-end test for the transaction forwarding pipeline.
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
async fn test_tx_forwarding_pipeline_e2e() -> Result<()> {
    // Build devnet with tx forwarding enabled on the client
    // The client will forward transactions to the builder's RPC endpoint
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            // Empty vector here because the stack will populate it with the builder RPC URL on start
            TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100),
        )
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

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

    Ok(())
}

/// Tests that the forwarding pipeline handles high transaction load under rate limiting.
///
/// Uses all 4 available test accounts (`ANVIL_ACCOUNT_1` through `ANVIL_ACCOUNT_4`) to send
/// transactions concurrently, with `max_rps = 1` forcing the forwarder to buffer heavily.
/// Each account sends 10 transactions for a total of 40, verifying that all are eventually
/// forwarded to the builder and included in blocks despite the constrained send rate.
#[tokio::test]
async fn test_tx_forwarding_pipeline_e2e_high_load() -> Result<()> {
    const TXS_PER_ACCOUNT: usize = 10;

    let accounts = [&*ANVIL_ACCOUNT_1, &*ANVIL_ACCOUNT_2, &*ANVIL_ACCOUNT_3, &*ANVIL_ACCOUNT_4];

    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_max_rps(1).with_resend_after_ms(30_000), // high resend window so we don't double-send
        )
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

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
            hex.parse().expect("valid private key")
        })
        .collect();

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

    Ok(())
}
