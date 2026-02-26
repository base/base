#![allow(missing_docs)]

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use base_builder_core::{
    BuilderConfig, FlashblocksConfig,
    test_utils::{TransactionBuilderExt, funded_signer, setup_test_instance_with_builder_config},
};
use serde_json::Value;

/// Verify that flashblock metadata contains correct `new_account_balances` and `receipts`.
///
/// Regression test for the bug where `new_account_balances` was read after
/// `take_bundle()` emptied the bundle state, producing an always-empty map.
#[tokio::test]
async fn test_flashblock_metadata_balances_and_receipts() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(true).with_leeway_time_ms(50);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    let sender = funded_signer().address();
    let transfer_value = U256::from(42);

    // Use high addresses to avoid overlap with EVM precompiles or OP predeploys
    let recipient_a = "0xAA00000000000000000000000000000000000001".parse::<Address>()?;
    let recipient_b = "0xBB00000000000000000000000000000000000002".parse::<Address>()?;
    let tx_a = driver
        .create_transaction()
        .with_to(recipient_a)
        .with_value(transfer_value.to::<u128>())
        .send()
        .await?;
    let tx_b = driver
        .create_transaction()
        .with_to(recipient_b)
        .with_value(transfer_value.to::<u128>())
        .send()
        .await?;

    let _block = driver.build_new_block().await?;

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert!(!flashblocks.is_empty(), "Expected at least one flashblock");

    // --- Verify new_account_balances on the last flashblock (cumulative state) ---
    let last_fb = flashblocks.last().unwrap();
    let balances = last_fb
        .metadata
        .get("new_account_balances")
        .expect("metadata should contain new_account_balances");
    let balances_map = balances.as_object().expect("new_account_balances should be an object");
    assert!(!balances_map.is_empty(), "new_account_balances should not be empty");

    // Sender should appear with a reduced balance (spent value + gas)
    let sender_key = format!("{sender:#x}");
    assert!(balances_map.contains_key(&sender_key), "sender should appear in new_account_balances");

    // Recipients started at zero; their balance should equal the transfer value
    for (label, recipient) in [("A", recipient_a), ("B", recipient_b)] {
        let key = format!("{recipient:#x}");
        let balance_value = balances_map
            .get(&key)
            .unwrap_or_else(|| panic!("recipient {label} ({key}) should appear in balances"));

        let balance_str = balance_value.as_str().unwrap_or_default();
        let balance = U256::from_str_radix(balance_str.trim_start_matches("0x"), 16)
            .expect("balance should be a valid hex U256");
        assert_eq!(
            balance, transfer_value,
            "recipient {label} should have exactly {transfer_value} wei"
        );
    }

    // --- Verify receipts across all flashblocks ---
    // Each flashblock's receipts only cover the transactions added in that flashblock,
    // so we collect receipts from all flashblocks.
    let tx_a_key = format!("{:#x}", tx_a.tx_hash());
    let tx_b_key = format!("{:#x}", tx_b.tx_hash());

    let has_receipt = |tx_key: &str| {
        flashblocks.iter().any(|fb| {
            fb.metadata
                .get("receipts")
                .and_then(|r| r.as_object())
                .is_some_and(|map| map.contains_key(tx_key))
        })
    };

    assert!(has_receipt(&tx_a_key), "receipt for tx_a ({tx_a_key}) should be present");
    assert!(has_receipt(&tx_b_key), "receipt for tx_b ({tx_b_key}) should be present");

    flashblocks_listener.stop().await
}

/// Test that when `compute_state_root_on_finalize` is enabled:
/// 1. Flashblocks are built without state root (`state_root` = ZERO in intermediate blocks)
/// 2. The final payload returned by `get_payload` has a valid state root (non-zero)
#[tokio::test]
async fn test_state_root_computed_on_finalize() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests()
        .with_fixed(true)
        .with_disable_state_root(true)
        .with_compute_state_root_on_finalize(true);
    let config = BuilderConfig::for_tests().with_block_time_ms(2000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // Send some transactions
    for _ in 0..3 {
        let _ = driver.create_transaction().random_valid_transfer().send().await?;
    }

    // Build a block with current timestamp to ensure payload doesn't expire
    let block = driver.build_new_block_with_current_timestamp(None).await?;

    // Verify the block has transactions
    assert_eq!(
        block.transactions.len(),
        4, // 3 user txs + 1 deposit
        "Block should contain deposit + user transactions"
    );

    // Verify that the FINAL block has a valid (non-zero) state root
    // when compute_state_root_on_finalize is enabled
    assert_ne!(
        block.header.state_root,
        B256::ZERO,
        "Final block state root should NOT be zero when compute_state_root_on_finalize is enabled"
    );

    // Verify flashblocks were produced
    let flashblocks = flashblocks_listener.get_flashblocks();
    assert!(!flashblocks.is_empty(), "Flashblocks should have been produced");

    // Verify intermediate flashblocks have zero state root (they skip state root calculation)
    for fb in &flashblocks {
        assert_eq!(
            fb.diff.state_root,
            B256::ZERO,
            "Intermediate flashblocks should have zero state root"
        );
    }

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn smoke_dynamic_base() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(false);
    let config = BuilderConfig::for_tests().with_block_time_ms(2000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align our block timestamps with current unix timestamp
    for _ in 0..10 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = driver.create_transaction().random_valid_transfer().send().await?;
        }
        let block = driver.build_new_block_with_current_timestamp(None).await?;
        assert_eq!(block.transactions.len(), 6, "Got: {:?}", block.transactions); // 5 normal txn + deposit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(110, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn smoke_dynamic_unichain() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(false);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align our block timestamps with current unix timestamp
    for _ in 0..10 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = driver.create_transaction().random_valid_transfer().send().await?;
        }
        let block = driver.build_new_block_with_current_timestamp(None).await?;
        assert_eq!(block.transactions.len(), 6, "Got: {:?}", block.transactions); // 5 normal txn + deposit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(60, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn smoke_classic_unichain() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(true).with_leeway_time_ms(50);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align our block timestamps with current unix timestamp
    for _ in 0..10 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = driver.create_transaction().random_valid_transfer().send().await?;
        }
        let block = driver.build_new_block().await?;
        assert_eq!(block.transactions.len(), 6, "Got: {:?}", block.transactions); // 5 normal txn + deposit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(60, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn smoke_classic_base() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(true).with_leeway_time_ms(50);
    let config = BuilderConfig::for_tests().with_block_time_ms(2000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    for _ in 0..10 {
        for _ in 0..5 {
            let _ = driver.create_transaction().random_valid_transfer().send().await?;
        }
        // Use current timestamp to prevent payload expiration with 2s block time
        let block = driver.build_new_block_with_current_timestamp(None).await?;
        assert_eq!(block.transactions.len(), 6, "Got: {:?}", block.transactions); // 5 normal txn + deposit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(110, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn unichain_dynamic_with_lag() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(false);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align our block timestamps with current unix timestamp
    for i in 0..9 {
        for _ in 0..5 {
            // send a valid transaction
            let _ = driver.create_transaction().random_valid_transfer().send().await?;
        }
        let block = driver
            .build_new_block_with_current_timestamp(Some(Duration::from_millis(i * 100)))
            .await?;
        assert_eq!(block.transactions.len(), 6, "Got: {:#?}", block.transactions); // 5 normal txn + deposit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(34, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn dynamic_with_full_block_lag() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests().with_fixed(false).with_leeway_time_ms(0);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    for _ in 0..5 {
        // send a valid transaction
        let _ = driver.create_transaction().random_valid_transfer().send().await?;
    }
    let block =
        driver.build_new_block_with_current_timestamp(Some(Duration::from_millis(999))).await?;
    // We could only produce block with deposits because of short time frame
    assert_eq!(block.transactions.len(), 1);

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert_eq!(1, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn test_flashblocks_no_state_root_calculation() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests()
        .with_fixed(false)
        .with_disable_state_root(true)
        .with_compute_state_root_on_finalize(false);
    let config = BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;

    // Send a transaction to ensure block has some activity
    let _tx = driver.create_transaction().random_valid_transfer().send().await?;

    // Build a block with current timestamp (not historical) and disable_state_root: true
    let block = driver.build_new_block_with_current_timestamp(None).await?;

    // Verify that flashblocks are still produced (block should have transactions)
    assert_eq!(block.transactions.len(), 2, "Block should contain deposit + user transaction");

    // Verify that state root is not calculated (should be zero)
    assert_eq!(
        block.header.state_root,
        B256::ZERO,
        "State root should be zero when disable_state_root is true"
    );

    Ok(())
}

/// Verify that flashblock metadata contains non-empty `new_account_balances`
/// when transactions that transfer value are included in the block.
#[tokio::test]
async fn test_flashblock_metadata_new_account_balances() -> eyre::Result<()> {
    let flashblocks_config = FlashblocksConfig::for_tests().with_fixed(false);
    let config =
        BuilderConfig::for_tests().with_block_time_ms(1000).with_flashblocks(flashblocks_config);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // Send a value transfer so at least sender + receiver have balance changes
    let _ = driver.create_transaction().random_valid_transfer().send().await?;

    let _block = driver.build_new_block_with_current_timestamp(None).await?;

    let flashblocks = flashblocks_listener.get_flashblocks();
    assert!(!flashblocks.is_empty(), "should have produced flashblocks");

    // At least one flashblock must have non-empty new_account_balances
    let has_balances = flashblocks.iter().any(|fb| {
        fb.metadata
            .get("new_account_balances")
            .and_then(Value::as_object)
            .is_some_and(|b| !b.is_empty())
    });

    assert!(has_balances, "at least one flashblock should report non-empty new_account_balances");

    flashblocks_listener.stop().await
}
