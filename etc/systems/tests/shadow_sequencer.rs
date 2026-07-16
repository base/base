//! System tests for shadow sequencers.
//!
//! A shadow sequencer builds real blocks from its own mempool but signs them
//! with a distinct key, so the rest of the network treats those blocks as
//! non-canonical. These tests assert that a shadow sequencer builds blocks
//! successfully while the canonical chain tip continues to reflect the active
//! sequencer.

use std::time::{Duration, Instant};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const NON_CANONICAL_OBSERVATION_WINDOW: Duration = Duration::from_secs(15);
const SHADOW_REORG_TIMEOUT: Duration = Duration::from_secs(30);

static SHADOW_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn shadow_builds_blocks_but_canonical_tip_reflects_active() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_shadow_sequencers(1)
        .build()
        .await?;

    assert_eq!(system.shadow_sequencer_count(), 1, "expected exactly one shadow sequencer");

    let active_builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let shadow_builder = system.l2_shadow_builder_provider(0)?;

    wait_for_block(&active_builder, 2).await.wrap_err("active sequencer did not produce blocks")?;
    wait_for_block(&shadow_builder, 2).await.wrap_err("shadow sequencer did not produce blocks")?;

    let active_sender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
        .wrap_err("failed to parse active-path signer")?;
    wait_for_balance(&active_builder, active_sender.address()).await?;
    wait_for_balance(&client, active_sender.address()).await?;
    let canonical_nonce = client.get_transaction_count(active_sender.address()).await?;
    let canonical_tx =
        send_transfer(&client, &active_sender, canonical_nonce, dead_address(0x01)).await?;
    wait_for_receipt(&active_builder, canonical_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("canonical tx never landed on the active sequencer")?;
    wait_for_receipt(&client, canonical_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("canonical tx never landed on the client")?;

    // Send a transaction that only the shadow sequencer sees. The shadow builder
    // runs an isolated execution layer, so this transaction never reaches the
    // active sequencer's mempool.
    let shadow_sender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)
        .wrap_err("failed to parse shadow-path signer")?;
    wait_for_balance(&shadow_builder, shadow_sender.address()).await?;
    let shadow_nonce = shadow_builder.get_transaction_count(shadow_sender.address()).await?;
    let shadow_tx =
        send_transfer(&shadow_builder, &shadow_sender, shadow_nonce, dead_address(0x02)).await?;

    let shadow_receipt = wait_for_receipt(&shadow_builder, shadow_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("shadow sequencer failed to build a block containing the shadow-only tx")?;
    assert!(
        shadow_receipt.inner.block_number.is_some(),
        "shadow receipt should reference a block number",
    );

    assert_never_included(&active_builder, shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the active sequencer")?;
    assert_never_included(&client, shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the client")?;

    // TARGET BEHAVIOR (future PR): once the shadow sequencer follows the
    // canonical chain, it reorgs away its shadow-built blocks and forgets the
    // shadow-only tx. This assertion is expected to FAIL today because the
    // shadow reorg logic does not exist yet.
    wait_for_shadow_to_forget(&shadow_builder, shadow_tx, SHADOW_REORG_TIMEOUT)
        .await
        .wrap_err("shadow sequencer did not reorg to canonical and forget its shadow-only tx")?;

    Ok(())
}

const fn dead_address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

async fn send_transfer(
    provider: &RootProvider<Base>,
    signer: &PrivateKeySigner,
    nonce: u64,
    recipient: Address,
) -> Result<B256> {
    let tx_request = BaseTransactionRequest::default()
        .from(signer.address())
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
    let raw_tx: Bytes = signed_tx.encoded_2718().into();
    let expected_hash = *signed_tx.hash();

    let pending = provider.send_raw_transaction(&raw_tx).await.wrap_err("failed to send tx")?;
    assert_eq!(*pending.tx_hash(), expected_hash, "transaction hash mismatch");
    Ok(expected_hash)
}

async fn wait_for_block(provider: &RootProvider<Base>, min_block: u64) -> Result<u64> {
    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= min_block {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("block production timed out")?
}

async fn wait_for_balance(provider: &RootProvider<Base>, address: Address) -> Result<()> {
    timeout(Duration::from_secs(30), async {
        loop {
            if provider.get_balance(address).await? > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("timed out waiting for a funded account")?
}

async fn wait_for_receipt(
    provider: &RootProvider<Base>,
    tx_hash: B256,
    within: Duration,
) -> Result<base_common_rpc_types::BaseTransactionReceipt> {
    timeout(within, async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("transaction receipt timed out")?
}

async fn assert_never_included(
    provider: &RootProvider<Base>,
    tx_hash: B256,
    window: Duration,
) -> Result<()> {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if provider.get_transaction_receipt(tx_hash).await?.is_some() {
            eyre::bail!("transaction was included when it should not have been");
        }
        sleep(BLOCK_POLL_INTERVAL).await;
    }
    Ok(())
}

async fn wait_for_shadow_to_forget(
    provider: &RootProvider<Base>,
    tx_hash: B256,
    within: Duration,
) -> Result<()> {
    timeout(within, async {
        loop {
            if provider.get_transaction_receipt(tx_hash).await?.is_none() {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("shadow sequencer still reports the shadow-only tx as included")?
}
