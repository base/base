//! System smoke test for mempool inline meterBundle + gated forwarding.
//!
//! Verifies the pre-zeronet path:
//! 1. Client `eth_sendRawTransaction` kicks off in-process `meterBundle`
//! 2. Forwarder waits for a Ready response (`require_metering`)
//! 3. Builder `base_insertValidatedTransaction` receives the piggybacked response
//! 4. Tx is included and metering is present in the builder store

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::MeteringProvider;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{ANVIL_ACCOUNT_1, SystemTestStackBuilder};
use base_tx_forwarding::TxForwardingConfig;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(90);

fn create_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    recipient: Address,
) -> Result<(Bytes, alloy_primitives::B256)> {
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

    Ok((raw_tx, tx_hash))
}

/// End-to-end smoke: gated inline metering must succeed for the tx to forward and land.
#[tokio::test]
async fn test_inline_metering_forwarding_pipeline() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100),
        )
        .with_inline_metering()
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    let metering = system
        .metering_provider()
        .expect("inline metering should install a builder metering store")
        .clone();

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

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

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

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction to client")?;
    assert_eq!(*pending_tx.tx_hash(), expected_tx_hash, "Transaction hash mismatch");

    // If inline sim never becomes Ready, require_metering blocks forwarding and this times out.
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
    .wrap_err(
        "Transaction receipt timed out on builder — inline metering gate or forwarder may have failed",
    )?
    .wrap_err("Failed to get transaction receipt")?;

    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    // Piggybacked meterBundle response should have been inserted on the builder.
    timeout(Duration::from_secs(10), async {
        loop {
            if metering.get(&expected_tx_hash).is_some() {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for builder metering store to receive piggybacked response")??;

    let stored = metering.get(&expected_tx_hash).expect("metering present after wait");
    assert!(
        stored.total_gas_used > 0 || !stored.results.is_empty(),
        "stored metering should reflect a successful meterBundle, got {stored:?}"
    );

    Ok(())
}
