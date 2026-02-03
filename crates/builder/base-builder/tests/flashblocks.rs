#![allow(missing_docs)]

use std::time::Duration;

use alloy_primitives::B256;
use base_builder_cli::{FlashblocksArgs, OpRbuilderArgs};
use base_builder::test_utils::{TransactionBuilderExt, setup_test_instance_with_args};

/// Test that when `compute_state_root_on_finalize` is enabled:
/// 1. Flashblocks are built without state root (`state_root` = ZERO in intermediate blocks)
/// 2. The final payload returned by `get_payload` has a valid state root (non-zero)
#[tokio::test]
async fn test_state_root_computed_on_finalize() -> eyre::Result<()> {
    let args = OpRbuilderArgs {
        chain_block_time: 2000, // 2 second block time for more flashblocks
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 100,
            flashblocks_fixed: true,
            flashblocks_disable_state_root: true,
            flashblocks_compute_state_root_on_finalize: true,
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // Send some transactions
    for _ in 0..3 {
        let _ = driver.create_transaction().random_valid_transfer().send().await?;
    }

    // Build a block - this will trigger get_payload which should compute state root
    let block = driver.build_new_block().await?;

    // Verify the block has transactions
    assert_eq!(
        block.transactions.len(),
        4, // 3 user txs + 1 deposit
        "Block should contain deposit + user transactions"
    );

    // Verify that the FINAL block has a valid (non-zero) state root
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
    let args = OpRbuilderArgs {
        chain_block_time: 2000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 100,
            flashblocks_fixed: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align out block timestamps with current unix timestamp
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
    let args = OpRbuilderArgs {
        chain_block_time: 1000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 100,
            flashblocks_fixed: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align out block timestamps with current unix timestamp
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
    let args = OpRbuilderArgs {
        chain_block_time: 1000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 50,
            flashblocks_fixed: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align out block timestamps with current unix timestamp
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
    let args = OpRbuilderArgs {
        chain_block_time: 2000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 50,
            flashblocks_fixed: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align out block timestamps with current unix timestamp
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
    assert_eq!(110, flashblocks.len());

    flashblocks_listener.stop().await
}

#[tokio::test]
async fn unichain_dynamic_with_lag() -> eyre::Result<()> {
    let args = OpRbuilderArgs {
        chain_block_time: 1000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 100,
            flashblocks_fixed: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
    let driver = rbuilder.driver().await?;
    let flashblocks_listener = rbuilder.spawn_flashblocks_listener();

    // We align out block timestamps with current unix timestamp
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
    let args = OpRbuilderArgs {
        chain_block_time: 1000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 0,
            flashblocks_fixed: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
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
    let args = OpRbuilderArgs {
        chain_block_time: 1000,
        flashblocks: FlashblocksArgs {
            flashblocks_port: 0,
            flashblocks_addr: "127.0.0.1".into(),
            flashblocks_block_time: 200,
            flashblocks_leeway_time: 100,
            flashblocks_fixed: false,
            flashblocks_disable_state_root: true,
            flashblocks_compute_state_root_on_finalize: false,
        },
        ..Default::default()
    };
    let rbuilder = setup_test_instance_with_args(args).await?;
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
