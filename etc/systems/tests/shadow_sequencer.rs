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

use std::{num::NonZeroU64, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, SystemTestProviderExt, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
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
        .with_shadow_blocks_per_cycle(NonZeroU64::new(10).expect("nonzero"))
        .build()
        .await?;

    assert_eq!(system.shadow_sequencer_count(), 1, "expected exactly one shadow sequencer");

    let active_builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let shadow_builder = system.l2_shadow_builder_provider(0)?;

    active_builder
        .wait_for_block(2, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("active sequencer did not produce blocks")?;
    shadow_builder
        .wait_for_block(2, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("shadow sequencer did not produce blocks")?;

    let active_sender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
        .wrap_err("failed to parse active-path signer")?;
    active_builder.wait_for_balance(active_sender.address(), BALANCE_SYNC_TIMEOUT).await?;
    client.wait_for_balance(active_sender.address(), BALANCE_SYNC_TIMEOUT).await?;
    let canonical_nonce = client.get_transaction_count(active_sender.address()).await?;
    let canonical_tx =
        send_transfer(&active_builder, &active_sender, canonical_nonce, dead_address(0x01)).await?;
    active_builder
        .wait_for_receipt(canonical_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("canonical tx never landed on the active sequencer")?;
    client
        .wait_for_receipt(canonical_tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("canonical tx never landed on the client")?;

    // Send a transaction that only the shadow sequencer sees. The shadow builder
    // runs an isolated execution layer, so this transaction never reaches the
    // active sequencer's mempool.
    let shadow_sender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)
        .wrap_err("failed to parse shadow-path signer")?;
    shadow_builder.wait_for_balance(shadow_sender.address(), BALANCE_SYNC_TIMEOUT).await?;
    let shadow_nonce = shadow_builder.get_transaction_count(shadow_sender.address()).await?;
    let shadow_tx =
        send_transfer(&shadow_builder, &shadow_sender, shadow_nonce, dead_address(0x02)).await?;

    let shadow_receipt = shadow_builder
        .wait_for_receipt(shadow_tx, TX_RECEIPT_TIMEOUT)
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

    active_builder
        .assert_receipt_absent(shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the active sequencer")?;
    client
        .assert_receipt_absent(shadow_tx, NON_CANONICAL_OBSERVATION_WINDOW)
        .await
        .wrap_err("shadow-only tx unexpectedly appeared on the client")?;

    // The shadow built a distinct private block: the canonical chain never saw the
    // shadow-only tx, so its block at `divergence_height` must differ from the
    // shadow's private block that included it.
    let canonical_hash_at_divergence = active_builder
        .wait_for_block_hash_at(divergence_height, BLOCK_PRODUCTION_TIMEOUT)
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
    shadow_builder
        .wait_for_convergence(&active_builder, divergence_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("shadow did not reconcile its private block to the canonical chain")?;

    // Adopting canonical payloads means the shadow now serves the canonical tx it
    // never saw in its own mempool.
    shadow_builder
        .wait_for_receipt(canonical_tx, TX_RECEIPT_TIMEOUT)
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
    active_builder
        .wait_for_block(target_height + 2, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("active sequencer did not advance far enough")?;

    shadow_builder
        .wait_for_convergence(&active_builder, target_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("shadow did not stay converged to canonical across multiple cycles")?;

    Ok(())
}

#[tokio::test]
async fn late_shadow_catches_up_then_reconciles_private_blocks() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    let start_height = 3;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_shadow_sequencers(1)
        .with_shadow_blocks_per_cycle(NonZeroU64::new(3).expect("nonzero"))
        .with_shadow_start_block(start_height)
        .build()
        .await?;

    let active_builder = system.l2_builder_provider()?;
    let shadow_builder = system.l2_shadow_builder_provider(0)?;
    let active_height = active_builder.get_block_number().await?;
    assert!(
        active_height >= start_height,
        "active must reach the requested height before the late shadow is operational"
    );

    let pre_shadow_height = start_height - 1;
    let canonical_pre_shadow_hash =
        active_builder.wait_for_block_hash_at(pre_shadow_height, BLOCK_PRODUCTION_TIMEOUT).await?;
    shadow_builder
        .wait_for_convergence(&active_builder, pre_shadow_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("late shadow did not catch up a block produced before it started")?;
    assert_eq!(
        shadow_builder.block_hash_at(pre_shadow_height).await?,
        Some(canonical_pre_shadow_hash),
        "late shadow must import pre-start canonical history"
    );

    let safe_height = active_builder
        .get_block_by_number(BlockNumberOrTag::Safe)
        .await?
        .map_or(0, |block| block.header.number);
    let post_start_height = safe_height + 1;
    assert!(
        safe_height < post_start_height,
        "post-start handoff block must still be unsafe: safe={safe_height}, \
         handoff={post_start_height}"
    );
    let handoff_result = shadow_builder
        .wait_for_convergence(&active_builder, post_start_height, Duration::from_secs(30))
        .await;
    if handoff_result.is_err() {
        let shadow_height = shadow_builder.get_block_number().await?;
        let active_height = active_builder.get_block_number().await?;
        eyre::bail!(
            "late shadow did not reach canonical unsafe handoff block {post_start_height}: \
             shadow_height={shadow_height}, active_height={active_height}, safe_height={safe_height}"
        );
    }

    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)
        .wrap_err("failed to parse late-shadow signer")?;
    shadow_builder.wait_for_balance(signer.address(), BALANCE_SYNC_TIMEOUT).await?;
    let nonce = shadow_builder.get_transaction_count(signer.address()).await?;
    let shadow_tx = send_transfer(&shadow_builder, &signer, nonce, dead_address(0x03)).await?;
    let receipt = match shadow_builder.wait_for_receipt(shadow_tx, Duration::from_secs(15)).await {
        Ok(receipt) => receipt,
        Err(error) => {
            let shadow_height = shadow_builder.get_block_number().await?;
            let active_height = active_builder.get_block_number().await?;
            eyre::bail!(
                "late shadow did not include its private transaction: shadow_height={shadow_height}, \
                 active_height={active_height}, error={error}"
            );
        }
    };
    let private_height = receipt
        .inner
        .block_number
        .ok_or_else(|| eyre::eyre!("late-shadow receipt should reference a block number"))?;
    let private_hash = receipt
        .inner
        .block_hash
        .ok_or_else(|| eyre::eyre!("late-shadow receipt should reference a block hash"))?;
    let canonical_hash =
        active_builder.wait_for_block_hash_at(private_height, BLOCK_PRODUCTION_TIMEOUT).await?;
    assert_ne!(private_hash, canonical_hash, "late shadow must enter a private build cycle");

    shadow_builder
        .wait_for_convergence(&active_builder, private_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("late shadow did not reconcile its private cycle to canonical")?;

    let second_receipt = shadow_builder
        .wait_for_receipt_after(shadow_tx, private_height, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("shadow-only transaction was not re-included in the next private cycle")?;
    let second_private_height = second_receipt
        .inner
        .block_number
        .ok_or_else(|| eyre::eyre!("second shadow receipt should reference a block number"))?;
    let second_private_hash = second_receipt
        .inner
        .block_hash
        .ok_or_else(|| eyre::eyre!("second shadow receipt should reference a block hash"))?;
    let second_canonical_hash = active_builder
        .wait_for_block_hash_at(second_private_height, BLOCK_PRODUCTION_TIMEOUT)
        .await?;
    assert_ne!(
        second_private_hash, second_canonical_hash,
        "shadow-only transaction must force divergence in the second private cycle"
    );

    shadow_builder
        .wait_for_convergence(&active_builder, second_private_height, SHADOW_CONVERGENCE_TIMEOUT)
        .await
        .wrap_err("late shadow did not complete its second reconciliation")?;

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
