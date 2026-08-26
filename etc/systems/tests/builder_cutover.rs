//! End-to-end test for the Denim payload-builder and block-time cutover.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{ANVIL_ACCOUNT_1, SystemTestStackBuilder};
use eyre::{OptionExt, Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const DENIM_ACTIVATION_BLOCK: u64 = 10;
const LAST_VERIFIED_BLOCK: u64 = DENIM_ACTIVATION_BLOCK + 4;
const BLOCK_TIMEOUT: Duration = Duration::from_secs(45);

#[tokio::test]
async fn cuts_over_builder_and_block_time_at_denim() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .with_payload_builder_cutover()
        .build()
        .await?;
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;

    let pre_cutover_receipt_block =
        send_transaction(&builder, &signer).await.wrap_err("pre-cutover transaction failed")?;
    assert!(
        pre_cutover_receipt_block < DENIM_ACTIVATION_BLOCK,
        "pre-cutover transaction landed at block {pre_cutover_receipt_block}"
    );

    wait_for_block(&builder, DENIM_ACTIVATION_BLOCK + 1).await?;
    let post_cutover_receipt_block =
        send_transaction(&builder, &signer).await.wrap_err("post-cutover transaction failed")?;
    assert!(
        post_cutover_receipt_block > DENIM_ACTIVATION_BLOCK,
        "post-cutover transaction landed at block {post_cutover_receipt_block}"
    );

    wait_for_block(&builder, LAST_VERIFIED_BLOCK).await?;
    wait_for_block(&client, LAST_VERIFIED_BLOCK).await?;
    verify_chain_and_cadence(&builder, &client).await?;

    Ok(())
}

async fn send_transaction(provider: &RootProvider<Base>, signer: &PrivateKeySigner) -> Result<u64> {
    let nonce = provider.get_transaction_count(signer.address()).await?;
    let transaction = BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xfe))
        .value(U256::from(1))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(2_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce)
        .build_typed_tx()
        .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let raw_transaction: Bytes = transaction.into_signed(signature).encoded_2718().into();
    let pending = provider.send_raw_transaction(&raw_transaction).await?;
    let transaction_hash = *pending.tx_hash();
    drop(pending);

    timeout(BLOCK_TIMEOUT, async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(transaction_hash).await? {
                return receipt.inner.block_number.ok_or_eyre("receipt missing block number");
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("transaction receipt timed out")?
}

async fn wait_for_block(provider: &RootProvider<Base>, target: u64) -> Result<()> {
    timeout(BLOCK_TIMEOUT, async {
        while provider.get_block_number().await? < target {
            sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, eyre::Error>(())
    })
    .await
    .wrap_err_with(|| format!("timed out waiting for block {target}"))?
}

async fn verify_chain_and_cadence(
    builder: &RootProvider<Base>,
    client: &RootProvider<Base>,
) -> Result<()> {
    let mut previous_hash = None;
    let mut previous_timestamp_ms = None;

    for number in 0..=LAST_VERIFIED_BLOCK {
        let builder_block = builder
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("builder block missing")?;
        let client_block = client
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("client block missing")?;
        assert_eq!(builder_block.header.hash, client_block.header.hash, "block {number} mismatch");

        if let Some(hash) = previous_hash {
            assert_eq!(builder_block.header.parent_hash, hash, "non-contiguous block {number}");
        }

        let timestamp_ms = builder_block
            .header
            .timestamp_ms
            .unwrap_or_else(|| builder_block.header.timestamp.saturating_mul(1_000));
        if let Some(previous) = previous_timestamp_ms {
            let expected_delta = if number <= DENIM_ACTIVATION_BLOCK { 2_000 } else { 200 };
            assert_eq!(timestamp_ms - previous, expected_delta, "block {number} cadence mismatch");
        }

        previous_hash = Some(builder_block.header.hash);
        previous_timestamp_ms = Some(timestamp_ms);
    }

    Ok(())
}
