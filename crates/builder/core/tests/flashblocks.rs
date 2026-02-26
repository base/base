#![allow(missing_docs)]

use std::time::Duration;

use alloy_primitives::B256;
use base_builder_core::{
    BuilderConfig, FlashblocksConfig,
    test_utils::{TransactionBuilderExt, setup_test_instance_with_builder_config},
};
use serde_json::Value;

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

/// Regression test: when `compute_state_root_on_finalize` is enabled and the builder
/// receives an FCU with `no_tx_pool = true` (historical sync), `resolve_kind` must still
/// return a payload. 
#[tokio::test]
async fn test_no_tx_pool_with_compute_state_root_on_finalize() -> eyre::Result<()> {
    let flashblocks = FlashblocksConfig::for_tests()
        .with_fixed(true)
        .with_disable_state_root(true)
        .with_compute_state_root_on_finalize(true);
    let config = BuilderConfig::for_tests().with_block_time_ms(2000).with_flashblocks(flashblocks);
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;

    // Seed the chain with a normal block first so we have a valid parent
    let _ = driver.build_new_block_with_current_timestamp(None).await?;

    // Build a block with no_tx_pool=true (historical sync).
    // Before the fix this would hang forever because finalized_cell was never set.
    let block = tokio::time::timeout(
        Duration::from_secs(30),
        driver.build_new_block_with_no_tx_pool(),
    )
    .await
    .expect("get_payload timed out — finalized_cell was likely never populated")?;

    // The block must contain the deposit transaction
    assert_eq!(
        block.transactions.len(),
        1,
        "no_tx_pool block should contain exactly one deposit transaction"
    );

    // State root should be valid (non-zero) because no_tx_pool forces state root calculation
    assert_ne!(
        block.header.state_root,
        B256::ZERO,
        "State root must be computed for CL sync (no_tx_pool path)"
    );

    Ok(())
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
