use crate::tests::{ChainDriverExt, LocalInstance, framework::ONE_ETH};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{TxHash, U256};
use alloy_provider::Provider;
use macros::rb_test;
use tips_core::{AcceptedBundle, MeterBundleResponse};
use uuid::Uuid;

/// Tests that backrun bundles are executed correctly:
/// - Backrun txs are included in the block after their target tx
/// - Backrun txs within a bundle are sorted by priority fee (highest first)
/// - The final block maintains correct ordering
#[rb_test(flashblocks)]
async fn backrun_bundles_execution(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    // 1. Build target tx first (we need Recovered<OpTxEnvelope> for bundle)
    let target_tx = driver
        .create_transaction()
        .with_signer(accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash().clone();

    // Send to mempool manually (send() doesn't return the Recovered tx)
    let provider = rbuilder.provider().await?;
    let _ = provider
        .send_raw_transaction(target_tx.encoded_2718().as_slice())
        .await?;

    // 2. Create backrun transactions with different priority fees
    //    We intentionally create backrun_low first to verify sorting reorders them
    let backrun_low = driver
        .create_transaction()
        .with_signer(accounts[1])
        .with_max_priority_fee_per_gas(10)
        .build()
        .await;
    let backrun_low_hash = backrun_low.tx_hash();

    let backrun_high = driver
        .create_transaction()
        .with_signer(accounts[2])
        .with_max_priority_fee_per_gas(50)
        .build()
        .await;
    let backrun_high_hash = backrun_high.tx_hash();

    // 3. Insert backrun bundle into store
    //    Bundle format: [target_tx, backrun_txs...]
    //    We include backrun_low BEFORE backrun_high to verify sorting reorders them
    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun_low, backrun_high],
        block_number: driver.latest().await?.header.number + 1,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
        meter_bundle_response: MeterBundleResponse {
            bundle_gas_price: U256::ZERO,
            bundle_hash: TxHash::ZERO,
            coinbase_diff: U256::ZERO,
            eth_sent_to_coinbase: U256::ZERO,
            gas_fees: U256::ZERO,
            results: vec![],
            state_block_number: 0,
            state_flashblock_index: None,
            total_gas_used: 0,
            total_execution_time_us: 0,
        },
    };

    rbuilder
        .backrun_bundle_store()
        .insert(bundle)
        .expect("Failed to insert backrun bundle");

    // 5. Build the block
    driver.build_new_block().await?;

    // 6. Verify block contents
    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    // Target tx should be in block
    assert!(
        tx_hashes.contains(&target_tx_hash),
        "Target tx not included in block"
    );

    // Both backrun txs should be in block
    assert!(
        tx_hashes.contains(&backrun_low_hash),
        "Backrun low priority tx not included in block"
    );
    assert!(
        tx_hashes.contains(&backrun_high_hash),
        "Backrun high priority tx not included in block"
    );

    // 7. Verify ordering: target < backrun_high < backrun_low
    //    (high priority fee should come before low priority fee)
    let target_pos = tx_hashes
        .iter()
        .position(|h| *h == target_tx_hash)
        .expect("Target tx position not found");
    let high_pos = tx_hashes
        .iter()
        .position(|h| *h == backrun_high_hash)
        .expect("Backrun high position not found");
    let low_pos = tx_hashes
        .iter()
        .position(|h| *h == backrun_low_hash)
        .expect("Backrun low position not found");

    assert!(
        target_pos < high_pos,
        "Target tx (pos {}) should come before high priority backrun (pos {})",
        target_pos,
        high_pos
    );
    assert!(
        high_pos < low_pos,
        "High priority backrun (pos {}) should come before low priority backrun (pos {})",
        high_pos,
        low_pos
    );

    Ok(())
}

/// Tests that backrun bundles are all-or-nothing:
/// - If any backrun tx in a bundle reverts, the entire bundle is excluded
/// - Even successful txs in the bundle are not included
#[rb_test(flashblocks)]
async fn backrun_bundle_all_or_nothing_revert(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    // 1. Build target tx first (we need Recovered<OpTxEnvelope> for bundle)
    let target_tx = driver
        .create_transaction()
        .with_signer(accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash().clone();

    // Send to mempool manually (send() doesn't return the Recovered tx)
    let provider = rbuilder.provider().await?;
    let _ = provider
        .send_raw_transaction(target_tx.encoded_2718().as_slice())
        .await?;

    // 2. Create backrun transactions:
    //    - backrun_ok: valid tx with HIGH priority (executes first, succeeds)
    //    - backrun_revert: tx that will REVERT with LOW priority (executes second, fails)
    let backrun_ok = driver
        .create_transaction()
        .with_signer(accounts[1])
        .with_max_priority_fee_per_gas(50) // High priority - executes first
        .build()
        .await;
    let backrun_ok_hash = backrun_ok.tx_hash().clone();

    let backrun_revert = driver
        .create_transaction()
        .with_signer(accounts[2])
        .with_max_priority_fee_per_gas(10) // Low priority - executes second
        .with_revert() // This tx will revert
        .build()
        .await;
    let backrun_revert_hash = backrun_revert.tx_hash().clone();

    // 3. Insert backrun bundle into store
    //    Bundle format: [target_tx, backrun_txs...]
    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun_ok, backrun_revert],
        block_number: driver.latest().await?.header.number + 1,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
        meter_bundle_response: MeterBundleResponse {
            bundle_gas_price: U256::ZERO,
            bundle_hash: TxHash::ZERO,
            coinbase_diff: U256::ZERO,
            eth_sent_to_coinbase: U256::ZERO,
            gas_fees: U256::ZERO,
            results: vec![],
            state_block_number: 0,
            state_flashblock_index: None,
            total_gas_used: 0,
            total_execution_time_us: 0,
        },
    };

    rbuilder
        .backrun_bundle_store()
        .insert(bundle)
        .expect("Failed to insert backrun bundle");

    // 4. Build the block
    driver.build_new_block().await?;

    // 5. Verify block contents
    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    // Target tx SHOULD be in block (it was in mempool independently)
    assert!(
        tx_hashes.contains(&target_tx_hash),
        "Target tx should be included in block"
    );

    // backrun_ok should NOT be in block (all-or-nothing: bundle failed)
    assert!(
        !tx_hashes.contains(&backrun_ok_hash),
        "backrun_ok should NOT be in block (all-or-nothing revert)"
    );

    // backrun_revert should NOT be in block (it caused the revert)
    assert!(
        !tx_hashes.contains(&backrun_revert_hash),
        "backrun_revert should NOT be in block"
    );

    Ok(())
}
