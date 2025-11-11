use alloy_consensus::Receipt;
use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, B256};
use alloy_provider::Provider;
use base_reth_flashblocks_rpc::rpc::FlashblocksAPI;
use base_reth_test_utils::harness::TestHarness;
use base_reth_test_utils::node::{default_launcher, BASE_CHAIN_ID};
use eyre::{eyre, Result};
use op_alloy_consensus::OpTxEnvelope;
use reth::providers::HeaderProvider;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_transaction_pool::test_utils::TransactionBuilder;
use tips_core::types::Bundle;

use super::utils::{build_single_flashblock, secret_from_hex};
use crate::rpc::{MeteringApiImpl, MeteringApiServer};

#[tokio::test]
async fn meters_bundle_after_advancing_blocks() -> Result<()> {
    reth_tracing::init_test_tracing();
    let harness = TestHarness::new(default_launcher).await?;

    let provider = harness.provider();
    let bob = &harness.accounts().bob;
    let alice_secret = secret_from_hex(harness.accounts().alice.private_key);

    let tx = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(0)
        .to(bob.address)
        .value(1)
        .gas_limit(21_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559();

    let envelope = OpTxEnvelope::from(OpTransactionSigned::Eip1559(
        tx.as_eip1559().unwrap().clone(),
    ));
    let tx_bytes = Bytes::from(envelope.encoded_2718());

    harness.advance_chain(1).await?;

    let bundle = Bundle {
        txs: vec![tx_bytes.clone()],
        block_number: provider.get_block_number().await?,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };

    let metering_api =
        MeteringApiImpl::new(harness.blockchain_provider(), harness.flashblocks_state());
    let response = MeteringApiServer::meter_bundle(&metering_api, bundle)
        .await
        .map_err(|err| eyre!("meter_bundle rpc failed: {}", err))?;

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.total_gas_used, 21_000);
    assert!(response.state_flashblock_index.is_none());

    Ok(())
}

#[tokio::test]
async fn pending_flashblock_updates_state() -> Result<()> {
    reth_tracing::init_test_tracing();
    let harness = TestHarness::new(default_launcher).await?;

    let provider = harness.provider();
    let bob = &harness.accounts().bob;
    let alice_secret = secret_from_hex(harness.accounts().alice.private_key);

    let tx = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(0)
        .to(bob.address)
        .value(1)
        .gas_limit(21_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559();

    let blockchain_provider = harness.blockchain_provider();
    let latest_number = provider.get_block_number().await?;
    let latest_header = blockchain_provider
        .sealed_header(latest_number)?
        .ok_or_else(|| eyre!("missing header for block {}", latest_number))?;

    let pending_block_number = latest_header.number + 1;
    let envelope = OpTxEnvelope::from(OpTransactionSigned::Eip1559(
        tx.as_eip1559().unwrap().clone(),
    ));
    let tx_hash = envelope.tx_hash();
    let tx_bytes = Bytes::from(envelope.encoded_2718());
    let receipt = OpReceipt::Eip1559(Receipt {
        status: true.into(),
        cumulative_gas_used: 21_000,
        logs: vec![],
    });

    // Use a zero parent beacon block root to emulate a flashblock that predates Cancun data,
    // which should cause metering to surface the missing-root error while still caching state.
    let flashblock = build_single_flashblock(
        pending_block_number,
        latest_header.hash(),
        B256::ZERO,
        latest_header.timestamp + 2,
        latest_header.gas_limit,
        vec![(tx_bytes.clone(), Some((tx_hash, receipt.clone())))],
    );

    harness.send_flashblock(flashblock).await?;

    let bundle = Bundle {
        txs: vec![tx_bytes.clone()],
        block_number: pending_block_number,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };

    let metering_api =
        MeteringApiImpl::new(blockchain_provider.clone(), harness.flashblocks_state());
    let result = MeteringApiServer::meter_bundle(&metering_api, bundle).await;

    let err = result.expect_err("pending flashblock metering should surface missing beacon root");
    assert!(
        err.message().contains("parent beacon block root missing"),
        "unexpected error: {err:?}"
    );

    let pending_blocks = harness.flashblocks_state().get_pending_blocks();
    assert!(pending_blocks.is_some(), "expected flashblock to populate pending state");
    assert_eq!(
        pending_blocks.as_ref().unwrap().latest_flashblock_index(),
        0
    );

    Ok(())
}
