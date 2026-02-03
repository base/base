//! Smoke tests for the full Devnet stack.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::TransactionBuilder;
use op_alloy_rpc_types::OpTransactionRequest;
use system_tests::{
    DevnetBuilder, L1_CHAIN_ID, L2_CHAIN_ID, config::ANVIL_ACCOUNT_1, http_provider,
};
use tokio::time::{sleep, timeout};

const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test]
async fn smoke_test_devnet_block_production_and_transactions() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let l1_rpc_url = devnet.l1_rpc_url().await?;
    let l2_builder_rpc_url = devnet.l2_rpc_url()?;
    let l2_client_rpc_url = devnet.l2_client_rpc_url()?;

    verify_l1_block_production(l1_rpc_url.as_ref()).await?;
    verify_l2_block_production(l2_builder_rpc_url.as_ref()).await?;
    send_l2_transaction_via_client(l2_client_rpc_url.as_ref(), l2_builder_rpc_url.as_ref()).await?;

    Ok(())
}

async fn verify_l1_block_production(rpc_url: &str) -> Result<()> {
    let provider = http_provider(rpc_url)?;

    let initial_block = provider.get_block_number().await?;

    let result = timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            sleep(BLOCK_POLL_INTERVAL).await;
            let current_block = provider.get_block_number().await?;
            if current_block > initial_block {
                return Ok::<_, eyre::Error>(current_block);
            }
        }
    })
    .await
    .wrap_err("L1 block production timed out")??;

    assert!(result > initial_block, "L1 should produce new blocks");
    Ok(())
}

async fn verify_l2_block_production(rpc_url: &str) -> Result<()> {
    let provider = http_provider(rpc_url)?;

    let initial_block = provider.get_block_number().await?;

    let result = timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            sleep(BLOCK_POLL_INTERVAL).await;
            let current_block = provider.get_block_number().await?;
            if current_block > initial_block {
                return Ok::<_, eyre::Error>(current_block);
            }
        }
    })
    .await
    .wrap_err("L2 block production timed out")??;

    assert!(result > initial_block, "L2 should produce new blocks");
    Ok(())
}

async fn send_l2_transaction_via_client(client_rpc_url: &str, builder_rpc_url: &str) -> Result<()> {
    let builder_provider = http_provider(builder_rpc_url)?;
    let client_provider = http_provider(client_rpc_url)?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender_address = signer.address();

    let builder_balance = builder_provider.get_balance(sender_address).await?;
    assert!(builder_balance > U256::ZERO, "Sender should have balance on builder");

    timeout(Duration::from_secs(30), async {
        loop {
            let client_balance = client_provider.get_balance(sender_address).await?;
            if client_balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for client to sync balance")??;

    let nonce = client_provider.get_transaction_count(sender_address).await?;

    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let tx_request = OpTransactionRequest::default()
        .from(sender_address)
        .to(recipient)
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce);

    let tx = tx_request.build_typed_tx().map_err(|_| eyre::eyre!("invalid transaction request"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let raw_tx: alloy_primitives::Bytes = signed_tx.encoded_2718().into();
    let expected_tx_hash = *signed_tx.hash();

    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction")?;
    let tx_hash = *pending_tx.tx_hash();
    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) = builder_provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out on builder")?
    .wrap_err("Failed to get transaction receipt")?;

    assert_eq!(receipt.transaction_hash, tx_hash);
    assert!(receipt.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.from, sender_address);
    assert_eq!(receipt.to, Some(recipient));

    Ok(())
}

#[tokio::test]
async fn smoke_test_builder_and_client_block_sync() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let l2_builder_rpc_url = devnet.l2_rpc_url()?;
    let l2_client_rpc_url = devnet.l2_client_rpc_url()?;

    let builder_provider = http_provider(l2_builder_rpc_url.as_ref())?;
    let client_provider = http_provider(l2_client_rpc_url.as_ref())?;

    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = builder_provider.get_block_number().await?;
            if block >= 3 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Builder block production timed out")??;

    let client_block = timeout(Duration::from_secs(60), async {
        loop {
            let client_block = client_provider.get_block_number().await?;
            if client_block > 0 {
                return Ok::<_, eyre::Error>(client_block);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Client block sync timed out - client stayed at block 0")??;

    assert!(client_block > 0, "Client should have synced at least one block");

    Ok(())
}

#[tokio::test]
async fn smoke_test_client_pending_state_via_flashblocks() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let l2_builder_rpc_url = devnet.l2_rpc_url()?;
    let l2_client_rpc_url = devnet.l2_client_rpc_url()?;

    let builder_provider = http_provider(l2_builder_rpc_url.as_ref())?;
    let client_provider = http_provider(l2_client_rpc_url.as_ref())?;

    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = builder_provider.get_block_number().await?;
            if block >= 3 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Builder block production timed out")??;

    timeout(Duration::from_secs(60), async {
        loop {
            let client_block = client_provider.get_block_number().await?;
            if client_block >= 1 {
                return Ok::<_, eyre::Error>(client_block);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Client block sync timed out")??;

    let mut matches = 0;
    let required_matches = 3;

    for _ in 0..10 {
        let builder_pending = get_pending_block_number(&builder_provider).await?;
        let client_pending = get_pending_block_number(&client_provider).await?;

        let diff = (builder_pending as i64 - client_pending as i64).abs();

        if diff <= 1 {
            matches += 1;
            if matches >= required_matches {
                return Ok(());
            }
        } else {
            matches = 0;
        }

        sleep(Duration::from_millis(250)).await;
    }

    eyre::bail!(
        "Client pending state not tracking builder - only got {matches}/{required_matches} matches"
    );
}

async fn get_pending_block_number<P: Provider>(provider: &P) -> Result<u64> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Pending)
        .await
        .wrap_err("Failed to get pending block")?;

    match block {
        Some(b) => Ok(b.header.number),
        None => Ok(0),
    }
}
