//! System tests for shadow sequencers.
//!
//! A shadow sequencer builds real blocks from its own mempool but signs them
//! with a distinct key, so the rest of the network treats those blocks as
//! non-canonical. It also runs with `shadow_blocks_per_cycle` set, so after
//! building a cycle of private blocks it reconciles back to the active
//! sequencer's canonical chain: it reorgs away its private blocks and adopts the
//! canonical payloads it buffered from gossip.
//!
//! These tests assert both halves of that behavior: shadow-only transactions
//! never leak onto the canonical chain, and the shadow eventually converges to
//! the canonical chain (adopting canonical blocks and discarding its private
//! ones).

use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::{BaseTransactionReceipt, BaseTransactionRequest};
use base_system_tests::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const BALANCE_SYNC_TIMEOUT: Duration = Duration::from_secs(30);
const NON_CANONICAL_OBSERVATION_WINDOW: Duration = Duration::from_secs(15);
const SHADOW_CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(90);

static SHADOW_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn shadow_builds_privately_then_reconciles_to_canonical() -> Result<()> {
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
    let divergence_height = shadow_receipt
        .inner
        .block_number
        .ok_or_else(|| eyre::eyre!("shadow receipt should reference a block number"))?;
    let shadow_private_hash = shadow_receipt
        .inner
        .block_hash
        .ok_or_else(|| eyre::eyre!("shadow receipt should reference a block hash"))?;

    assert_never_included(&active_builder, shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the active sequencer")?;
    assert_never_included(&client, shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the client")?;

    // The shadow built a distinct private block: the canonical chain never saw the
    // shadow-only tx, so its block at `divergence_height` must differ from the
    // shadow's private block that included it.
    let canonical_hash_at_divergence =
        wait_for_block_hash_at(&active_builder, divergence_height, BLOCK_PRODUCTION_TIMEOUT)
            .await
            .wrap_err("active sequencer did not reach the divergence height")?;
    assert_ne!(
        shadow_private_hash, canonical_hash_at_divergence,
        "shadow's private block should differ from the canonical block at the same height"
    );

    // Reconciliation: the shadow reorgs away its private block at
    // `divergence_height` and adopts the canonical block at that height. Proving
    // the shadow's block hash at that height now equals the canonical block hash
    // shows the private block (and its shadow-only tx) was discarded in favor of
    // canonical state — a robust check that does not race with the shadow-only tx
    // being re-injected into the shadow mempool after the reorg.
    wait_for_shadow_convergence(
        &shadow_builder,
        &active_builder,
        divergence_height,
        SHADOW_CONVERGENCE_TIMEOUT,
    )
    .await
    .wrap_err("shadow did not reconcile its private block to the canonical chain")?;

    // Adopting canonical payloads means the shadow now serves the canonical tx it
    // never saw in its own mempool.
    wait_for_receipt(&shadow_builder, canonical_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("shadow did not adopt the canonical tx after reconciliation")?;

    Ok(())
}

#[tokio::test]
async fn shadow_reconciles_across_multiple_cycles() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    let blocks_per_cycle = NonZeroU64::new(2).expect("nonzero");
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_shadow_sequencers(1)
        .with_shadow_blocks_per_cycle(blocks_per_cycle)
        .build()
        .await?;

    let active_builder = system.l2_builder_provider()?;
    let shadow_builder = system.l2_shadow_builder_provider(0)?;

    // A height that can only be reached after several reconciliation cycles have
    // advanced the shadow's anchor (3 cycles at 2 blocks/cycle).
    let target_height = blocks_per_cycle.get() * 3;
    wait_for_block(&active_builder, target_height + 2)
        .await
        .wrap_err("active sequencer did not advance far enough")?;

    wait_for_shadow_convergence(
        &shadow_builder,
        &active_builder,
        target_height,
        SHADOW_CONVERGENCE_TIMEOUT,
    )
    .await
    .wrap_err("shadow did not stay converged to canonical across multiple cycles")?;

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

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("invalid transaction request: {e:?}"))?;
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
    timeout(BALANCE_SYNC_TIMEOUT, async {
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
) -> Result<BaseTransactionReceipt> {
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

async fn block_hash_at(provider: &RootProvider<Base>, height: u64) -> Result<Option<B256>> {
    Ok(provider
        .get_block_by_number(BlockNumberOrTag::Number(height))
        .await?
        .map(|block| block.header.hash))
}

async fn wait_for_block_hash_at(
    provider: &RootProvider<Base>,
    height: u64,
    within: Duration,
) -> Result<B256> {
    timeout(within, async {
        loop {
            if let Some(hash) = block_hash_at(provider, height).await? {
                return Ok::<_, eyre::Error>(hash);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("block at target height not available in time")?
}

async fn wait_for_shadow_convergence(
    shadow: &RootProvider<Base>,
    canonical: &RootProvider<Base>,
    height: u64,
    within: Duration,
) -> Result<()> {
    timeout(within, async {
        loop {
            let canonical_hash = block_hash_at(canonical, height).await?;
            let shadow_hash = block_hash_at(shadow, height).await?;
            if let (Some(canonical_hash), Some(shadow_hash)) = (canonical_hash, shadow_hash)
                && canonical_hash == shadow_hash
            {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("shadow chain did not converge to canonical at the target height")?
}
