//! Integration tests for tips system.

#[path = "common/mod.rs"]
mod common;

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, TxHash, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use anyhow::{Context, Result, bail};
use base_primitives::{Bundle, BundleExtensions};
use common::kafka::wait_for_audit_event_by_hash;
use op_alloy_network::Optimism;
use serial_test::serial;
use tips_audit_lib::BundleEvent;
use tips_system_tests::{
    TipsRpcClient, create_funded_signer, create_optimism_provider, create_signed_transaction,
};
use tokio::time::{Duration, Instant, sleep};

/// Get the URL for integration tests against the TIPS ingress service
fn get_integration_test_url() -> String {
    std::env::var("INGRESS_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

/// Get the URL for the sequencer (for fetching nonces)
fn get_sequencer_url() -> String {
    std::env::var("SEQUENCER_URL").unwrap_or_else(|_| "http://localhost:8547".to_string())
}

async fn wait_for_transaction_seen(
    provider: &RootProvider<Optimism>,
    tx_hash: TxHash,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if Instant::now() >= deadline {
            bail!("Timed out waiting for transaction {tx_hash} to appear on the sequencer");
        }

        if provider.get_transaction_by_hash(tx_hash).await?.is_some() {
            return Ok(());
        }

        sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn test_client_can_connect_to_tips() -> Result<()> {
    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!(
            "Skipping integration tests (set INTEGRATION_TESTS=1 and ensure TIPS infrastructure is running)"
        );
        return Ok(());
    }

    let url = get_integration_test_url();
    let provider = create_optimism_provider(&url)?;
    let _client = TipsRpcClient::new(provider);
    Ok(())
}

#[tokio::test]
#[serial]
async fn test_send_raw_transaction_accepted() -> Result<()> {
    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!(
            "Skipping integration tests (set INTEGRATION_TESTS=1 and ensure TIPS infrastructure is running)"
        );
        return Ok(());
    }

    let url = get_integration_test_url();
    let provider = create_optimism_provider(&url)?;
    let client = TipsRpcClient::new(provider);
    let signer = create_funded_signer();

    let sequencer_url = get_sequencer_url();
    let sequencer_provider = create_optimism_provider(&sequencer_url)?;
    let nonce = sequencer_provider.get_transaction_count(signer.address()).await?;

    let to = Address::from([0x11; 20]);
    let value = U256::from(1000);
    let gas_limit = 21000;
    let gas_price = 1_000_000_000;

    let signed_tx = create_signed_transaction(&signer, to, value, nonce, gas_limit, gas_price)?;

    // Send transaction to TIPS
    let tx_hash = client
        .send_raw_transaction(signed_tx)
        .await
        .context("Failed to send transaction to TIPS")?;

    // Verify TIPS accepted the transaction and returned a hash
    assert!(!tx_hash.is_zero(), "Transaction hash should not be zero");

    // Verify transaction lands on-chain
    wait_for_transaction_seen(&sequencer_provider, tx_hash, 30)
        .await
        .context("Transaction never appeared on sequencer")?;

    // Verify transaction receipt shows success
    let receipt = sequencer_provider
        .get_transaction_receipt(tx_hash)
        .await
        .context("Failed to fetch transaction receipt")?
        .expect("Transaction receipt should exist after being seen on sequencer");
    assert!(receipt.status(), "Transaction should have succeeded");

    Ok(())
}

#[tokio::test]
#[serial]
async fn test_send_bundle_accepted() -> Result<()> {
    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!(
            "Skipping integration tests (set INTEGRATION_TESTS=1 and ensure TIPS infrastructure is running)"
        );
        return Ok(());
    }

    let url = get_integration_test_url();
    let provider = create_optimism_provider(&url)?;
    let client = TipsRpcClient::new(provider);
    let signer = create_funded_signer();

    let sequencer_url = get_sequencer_url();
    let sequencer_provider = create_optimism_provider(&sequencer_url)?;
    let nonce = sequencer_provider.get_transaction_count(signer.address()).await?;

    let to = Address::from([0x11; 20]);
    let value = U256::from(1000);
    let gas_limit = 21000;
    let gas_price = 1_000_000_000;

    let signed_tx = create_signed_transaction(&signer, to, value, nonce, gas_limit, gas_price)?;
    let tx_hash = keccak256(&signed_tx);

    // First send the transaction to mempool
    let _mempool_tx_hash = client
        .send_raw_transaction(signed_tx.clone())
        .await
        .context("Failed to send transaction to mempool")?;

    let bundle = Bundle {
        txs: vec![signed_tx],
        block_number: 0,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![tx_hash],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
        flashblock_number_min: None,
        flashblock_number_max: None,
    };

    // Send backrun bundle to TIPS
    let bundle_hash = client
        .send_backrun_bundle(bundle)
        .await
        .context("Failed to send backrun bundle to TIPS")?;

    // Verify TIPS accepted the bundle and returned a hash
    assert!(!bundle_hash.bundle_hash.is_zero(), "Bundle hash should not be zero");

    // Verify bundle hash is calculated correctly: keccak256(concat(tx_hashes))
    let mut concatenated = Vec::new();
    concatenated.extend_from_slice(tx_hash.as_slice());
    let expected_bundle_hash = keccak256(&concatenated);
    assert_eq!(
        bundle_hash.bundle_hash, expected_bundle_hash,
        "Bundle hash should match keccak256(tx_hash)"
    );

    // Verify audit channel emitted a Received event for this bundle
    let audit_event: BundleEvent =
        wait_for_audit_event_by_hash(&bundle_hash.bundle_hash, |event| {
            matches!(event, BundleEvent::Received { .. })
        })
        .await
        .context("Failed to read audit event from Kafka")?;
    match audit_event {
        BundleEvent::Received { bundle, .. } => {
            assert_eq!(
                bundle.bundle_hash(),
                bundle_hash.bundle_hash,
                "Audit event bundle hash should match response"
            );
        }
        other => panic!("Expected Received audit event, got {other:?}"),
    }

    // Wait for transaction to appear on sequencer
    wait_for_transaction_seen(&sequencer_provider, tx_hash, 60)
        .await
        .context("Bundle transaction never appeared on sequencer")?;

    // Verify transaction receipt shows success
    let receipt = sequencer_provider
        .get_transaction_receipt(tx_hash)
        .await
        .context("Failed to fetch transaction receipt")?
        .expect("Transaction receipt should exist after being seen on sequencer");
    assert!(receipt.status(), "Transaction should have succeeded");
    assert!(receipt.block_number().is_some(), "Transaction should be included in a block");

    Ok(())
}

#[tokio::test]
#[serial]
async fn test_send_bundle_with_two_transactions() -> Result<()> {
    if std::env::var("INTEGRATION_TESTS").is_err() {
        eprintln!(
            "Skipping integration tests (set INTEGRATION_TESTS=1 and ensure TIPS infrastructure is running)"
        );
        return Ok(());
    }

    let url = get_integration_test_url();
    let provider = create_optimism_provider(&url)?;
    let client = TipsRpcClient::new(provider);
    let signer = create_funded_signer();

    let sequencer_url = get_sequencer_url();
    let sequencer_provider = create_optimism_provider(&sequencer_url)?;
    let nonce = sequencer_provider.get_transaction_count(signer.address()).await?;

    // Create two transactions
    let tx1 = create_signed_transaction(
        &signer,
        Address::from([0x33; 20]),
        U256::from(1000),
        nonce,
        21000,
        1_000_000_000,
    )?;

    let tx2 = create_signed_transaction(
        &signer,
        Address::from([0x44; 20]),
        U256::from(2000),
        nonce + 1,
        21000,
        1_000_000_000,
    )?;

    let tx1_hash = keccak256(&tx1);
    let tx2_hash = keccak256(&tx2);

    // First send both transactions to mempool
    client.send_raw_transaction(tx1.clone()).await.context("Failed to send tx1 to mempool")?;
    client.send_raw_transaction(tx2.clone()).await.context("Failed to send tx2 to mempool")?;

    let bundle = Bundle {
        txs: vec![tx1, tx2],
        block_number: 0,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![tx1_hash, tx2_hash],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
        flashblock_number_min: None,
        flashblock_number_max: None,
    };

    // Send backrun bundle with 2 transactions to TIPS
    let bundle_hash = client
        .send_backrun_bundle(bundle)
        .await
        .context("Failed to send multi-transaction backrun bundle to TIPS")?;

    // Verify TIPS accepted the bundle and returned a hash
    assert!(!bundle_hash.bundle_hash.is_zero(), "Bundle hash should not be zero");

    // Verify bundle hash is calculated correctly: keccak256(concat(all tx_hashes))
    let mut concatenated = Vec::new();
    concatenated.extend_from_slice(tx1_hash.as_slice());
    concatenated.extend_from_slice(tx2_hash.as_slice());
    let expected_bundle_hash = keccak256(&concatenated);
    assert_eq!(
        bundle_hash.bundle_hash, expected_bundle_hash,
        "Bundle hash should match keccak256(concat(tx1_hash, tx2_hash))"
    );

    // Verify audit channel emitted a Received event
    let audit_event: BundleEvent =
        wait_for_audit_event_by_hash(&bundle_hash.bundle_hash, |event| {
            matches!(event, BundleEvent::Received { .. })
        })
        .await
        .context("Failed to read audit event for 2-tx bundle")?;
    match audit_event {
        BundleEvent::Received { bundle, .. } => {
            assert_eq!(
                bundle.bundle_hash(),
                bundle_hash.bundle_hash,
                "Audit event bundle hash should match response"
            );
        }
        other => panic!("Expected Received audit event, got {other:?}"),
    }

    // Wait for both transactions to appear on sequencer
    wait_for_transaction_seen(&sequencer_provider, tx1_hash, 60)
        .await
        .context("Bundle tx1 never appeared on sequencer")?;
    wait_for_transaction_seen(&sequencer_provider, tx2_hash, 60)
        .await
        .context("Bundle tx2 never appeared on sequencer")?;

    // Verify both transaction receipts show success
    for (tx_hash, name) in [(tx1_hash, "tx1"), (tx2_hash, "tx2")] {
        let receipt = sequencer_provider
            .get_transaction_receipt(tx_hash)
            .await
            .context(format!("Failed to fetch {name} receipt"))?
            .unwrap_or_else(|| panic!("{name} receipt should exist"));
        assert!(receipt.status(), "{name} should have succeeded");
        assert!(receipt.block_number().is_some(), "{name} should be included in a block");
    }

    Ok(())
}
