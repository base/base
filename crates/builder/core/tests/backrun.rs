#![allow(missing_docs)]

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{TxHash, U256};
use alloy_provider::Provider;
use base_builder_core::test_utils::{ChainDriverExt, ONE_ETH, setup_test_instance};
use base_bundles::{AcceptedBundle, MeterBundleResponse};
use uuid::Uuid;

/// Tests that backrun bundles are all-or-nothing:
/// - If any backrun tx in a bundle reverts, the entire bundle is excluded
/// - Even successful txs in the bundle are not included
#[tokio::test]
async fn backrun_bundle_all_or_nothing_revert() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    // 1. Build target tx first (we need Recovered<OpTxEnvelope> for bundle)
    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    // Send to mempool manually (send() doesn't return the Recovered tx)
    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    // 2. Create backrun transactions:
    //    - backrun_ok: valid tx with HIGH priority (executes first, succeeds)
    //    - backrun_revert: tx that will REVERT (executes second, fails)
    //    Both must have priority fee >= target's (20) to pass fee validation
    let backrun_ok = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(50) // High priority - executes first
        .build()
        .await;
    let backrun_ok_hash = backrun_ok.tx_hash();

    let backrun_revert = driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_max_priority_fee_per_gas(25) // >= target's 20, but executes second (lower than 50)
        .with_revert() // This tx will revert
        .build()
        .await;
    let backrun_revert_hash = backrun_revert.tx_hash();

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
            state_root_time_us: 0,
        },
    };

    rbuilder
        .tx_data_store()
        .insert_backrun_bundle(bundle)
        .expect("Failed to insert backrun bundle");

    // 4. Build the block
    driver.build_new_block().await?;

    // 5. Verify block contents
    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    // Target tx SHOULD be in block (it was in mempool independently)
    assert!(tx_hashes.contains(&target_tx_hash), "Target tx should be included in block");

    // backrun_ok should NOT be in block (all-or-nothing: bundle failed)
    assert!(
        !tx_hashes.contains(&backrun_ok_hash),
        "backrun_ok should NOT be in block (all-or-nothing revert)"
    );

    // backrun_revert should NOT be in block (it caused the revert)
    assert!(!tx_hashes.contains(&backrun_revert_hash), "backrun_revert should NOT be in block");

    Ok(())
}

/// Tests that only one backrun bundle executes per target tx, choosing the highest total priority fee
/// - Bundles are sorted by total priority fee (descending)
/// - Only the first successful bundle executes
/// - Lower priority bundles are not executed
#[tokio::test]
async fn backrun_bundles_sorted_by_total_fee() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(5, ONE_ETH).await?;

    // 1. Build target tx with priority fee 20
    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    // Send to mempool manually
    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    // 2. Create Bundle A with HIGH total priority fee
    //    Two txs: 60 + 50 = 110 total
    let bundle_a_tx1 = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(60)
        .build()
        .await;
    let bundle_a_tx1_hash = bundle_a_tx1.tx_hash();

    let bundle_a_tx2 = driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_max_priority_fee_per_gas(50)
        .build()
        .await;
    let bundle_a_tx2_hash = bundle_a_tx2.tx_hash();

    // 3. Create Bundle B with LOW total priority fee
    //    Two txs: 30 + 25 = 55 total
    let bundle_b_tx1 = driver
        .create_transaction()
        .with_signer(&accounts[3])
        .with_max_priority_fee_per_gas(30)
        .build()
        .await;
    let bundle_b_tx1_hash = bundle_b_tx1.tx_hash();

    let bundle_b_tx2 = driver
        .create_transaction()
        .with_signer(&accounts[4])
        .with_max_priority_fee_per_gas(25)
        .build()
        .await;
    let bundle_b_tx2_hash = bundle_b_tx2.tx_hash();

    // 4. Insert Bundle B FIRST (lower total fee), then Bundle A (higher total fee)
    //    This verifies that sorting reorders them correctly
    let bundle_b = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx.clone(), bundle_b_tx1, bundle_b_tx2],
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
            state_root_time_us: 0,
        },
    };

    let bundle_a = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, bundle_a_tx1, bundle_a_tx2],
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
            state_root_time_us: 0,
        },
    };

    // Insert in "wrong" order - B first, then A
    rbuilder.tx_data_store().insert_backrun_bundle(bundle_b).expect("Failed to insert bundle B");
    rbuilder.tx_data_store().insert_backrun_bundle(bundle_a).expect("Failed to insert bundle A");

    // 5. Build the block
    driver.build_new_block().await?;

    // 6. Verify block contents
    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    // Target tx should be in block
    assert!(tx_hashes.contains(&target_tx_hash), "Target tx not included in block");

    // Bundle A (higher total fee 110) should be in block
    assert!(
        tx_hashes.contains(&bundle_a_tx1_hash),
        "Bundle A tx1 should be included (highest priority bundle)"
    );
    assert!(
        tx_hashes.contains(&bundle_a_tx2_hash),
        "Bundle A tx2 should be included (highest priority bundle)"
    );

    // Bundle B (lower total fee 55) should NOT be in block (only one bundle executes per target)
    assert!(
        !tx_hashes.contains(&bundle_b_tx1_hash),
        "Bundle B tx1 should NOT be included (only one bundle per target tx)"
    );
    assert!(
        !tx_hashes.contains(&bundle_b_tx2_hash),
        "Bundle B tx2 should NOT be included (only one bundle per target tx)"
    );

    Ok(())
}

/// Tests that backrun bundles are rejected if total bundle priority fee < target tx priority fee
#[tokio::test]
async fn backrun_bundle_rejected_low_total_fee() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    // 1. Build target tx with HIGH priority fee (100)
    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    // Send to mempool manually
    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    // 2. Create backrun transactions with LOW total fee:
    //    - backrun_1: priority fee 30
    //    - backrun_2: priority fee 20
    //    - Total: 30 + 20 = 50 < target's 100 → bundle rejected
    let backrun_1 = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(30)
        .build()
        .await;
    let backrun_1_hash = backrun_1.tx_hash();

    let backrun_2 = driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let backrun_2_hash = backrun_2.tx_hash();

    // 3. Insert backrun bundle into store
    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun_1, backrun_2],
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
            state_root_time_us: 0,
        },
    };

    rbuilder
        .tx_data_store()
        .insert_backrun_bundle(bundle)
        .expect("Failed to insert backrun bundle");

    // 4. Build the block
    driver.build_new_block().await?;

    // 5. Verify block contents
    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    // Target tx SHOULD be in block (it was in mempool independently)
    assert!(tx_hashes.contains(&target_tx_hash), "Target tx should be included in block");

    // backrun_1 should NOT be in block (bundle rejected: total fee 50 < target fee 100)
    assert!(
        !tx_hashes.contains(&backrun_1_hash),
        "backrun_1 should NOT be in block (bundle rejected: total fee below target)"
    );

    // backrun_2 should NOT be in block (bundle rejected)
    assert!(!tx_hashes.contains(&backrun_2_hash), "backrun_2 should NOT be in block");

    Ok(())
}

#[tokio::test]
async fn backrun_bundle_rejected_exceeds_gas_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;

    // Set gas limit high enough for builder tx + target tx, but not backrun
    // Flashblocks has additional overhead, so use higher limits
    // Set limit to 500k, backrun requests 1M -> rejected
    driver.provider().raw_request::<(u64,), bool>("miner_setGasLimit".into(), (500_000,)).await?;

    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    let backrun = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(50)
        .with_gas_limit(1_000_000)
        .build()
        .await;
    let backrun_hash = backrun.tx_hash();

    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun],
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
            state_root_time_us: 0,
        },
    };

    rbuilder
        .tx_data_store()
        .insert_backrun_bundle(bundle)
        .expect("Failed to insert backrun bundle");

    driver.build_new_block().await?;

    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    assert!(tx_hashes.contains(&target_tx_hash), "Target tx should be included in block");

    assert!(
        !tx_hashes.contains(&backrun_hash),
        "Backrun should NOT be in block (exceeds gas limit)"
    );

    Ok(())
}

#[tokio::test]
async fn backrun_bundle_rejected_exceeds_da_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;

    // Set DA limit high enough for builder tx + target tx, but not backrun
    // Flashblocks has additional overhead, so use higher limits
    // Set block limit to 500 bytes, then create a backrun with large calldata
    driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 500))
        .await?;

    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    // Create backrun with large calldata to exceed DA limit
    let backrun = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(50)
        .with_input(vec![0u8; 1000].into())
        .build()
        .await;
    let backrun_hash = backrun.tx_hash();

    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun],
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
            state_root_time_us: 0,
        },
    };

    rbuilder
        .tx_data_store()
        .insert_backrun_bundle(bundle)
        .expect("Failed to insert backrun bundle");

    driver.build_new_block().await?;

    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    assert!(tx_hashes.contains(&target_tx_hash), "Target tx should be included in block");

    assert!(
        !tx_hashes.contains(&backrun_hash),
        "Backrun should NOT be in block (exceeds DA limit)"
    );

    Ok(())
}

/// Tests that backrun bundles with invalid tx errors (e.g. nonce too low) are skipped gracefully
#[tokio::test]
async fn backrun_bundle_invalid_tx_skipped() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    let target_tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_max_priority_fee_per_gas(20)
        .build()
        .await;
    let target_tx_hash = target_tx.tx_hash();

    let provider = rbuilder.provider().await?;
    let _ = provider.send_raw_transaction(target_tx.encoded_2718().as_slice()).await?;

    let backrun_tx = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(50)
        .with_nonce(0)
        .build()
        .await;
    let backrun_tx_hash = backrun_tx.tx_hash();

    // Send a conflicting tx with same nonce but higher fee - it will be included first
    let conflicting_tx = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_max_priority_fee_per_gas(100)
        .with_nonce(0)
        .send()
        .await?;
    let conflicting_tx_hash = *conflicting_tx.tx_hash();

    let bundle = AcceptedBundle {
        uuid: Uuid::new_v4(),
        txs: vec![target_tx, backrun_tx],
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
            state_root_time_us: 0,
        },
    };

    rbuilder
        .tx_data_store()
        .insert_backrun_bundle(bundle)
        .expect("Failed to insert backrun bundle");

    driver.build_new_block().await?;

    let block = driver.latest_full().await?;
    let tx_hashes: Vec<_> = block.transactions.hashes().collect();

    assert!(tx_hashes.contains(&target_tx_hash), "Target tx should be included in block");

    assert!(tx_hashes.contains(&conflicting_tx_hash), "Conflicting tx should be included in block");

    assert!(
        !tx_hashes.contains(&backrun_tx_hash),
        "Backrun tx should NOT be in block (nonce-too-low EVM error)"
    );

    Ok(())
}
