//! Tests for `max_uncompressed_block_size` limit enforcement and transaction spillover.

use alloy_primitives::{Address, Bytes};
use base_builder_core::{
    BuilderConfig,
    test_utils::{BlockTransactionsExt, setup_test_instance_with_builder_config},
};

/// Test that the uncompressed block size limit is enforced: only the highest priority fee
/// transaction fits per block, and excluded transactions spill into subsequent blocks
/// in priority fee order.
#[tokio::test]
async fn enforces_limits_uncompressed_size() -> eyre::Result<()> {
    // Set limit so only ~1 user tx fits per block (deposit tx + 1 user tx ≈ 100KB)
    let config = BuilderConfig::for_tests().with_max_uncompressed_block_size(Some(100_000));
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;

    let large_calldata = Bytes::from(vec![0u8; 50_000]);
    let small_calldata = Bytes::from(vec![0u8; 30_000]);

    let tx1 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(large_calldata.clone())
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(100)
        .send()
        .await?;

    // 30KB tx — small enough to fit in block 1 alongside tx1 within 100KB limit
    let tx_small = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(small_calldata)
        .with_gas_limit(400_000)
        .with_max_priority_fee_per_gas(95)
        .send()
        .await?;

    let tx2 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(large_calldata.clone())
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(90)
        .send()
        .await?;

    let tx3 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(large_calldata)
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(80)
        .send()
        .await?;

    // Build first block — tx1 (50KB, prio 100) + tx_small (30KB, prio 95) fit within 100KB,
    // but tx2 (50KB) would exceed the limit so it's excluded.
    let block1 = driver.build_new_block_with_current_timestamp(None).await?;
    assert!(block1.includes(tx1.tx_hash()), "tx1 should be included in block 1");
    assert!(block1.includes(tx_small.tx_hash()), "tx_small (30KB) should fit in block 1");
    assert!(!block1.includes(tx2.tx_hash()), "tx2 should be excluded from block 1");
    assert!(!block1.includes(tx3.tx_hash()), "tx3 should be excluded from block 1");

    // Build second block — tx2 (next highest priority) should be included
    let block2 = driver.build_new_block_with_current_timestamp(None).await?;
    assert!(block2.includes(tx2.tx_hash()), "tx2 excluded from block 1 should be in block 2");

    // Build third block — tx3 (lowest priority) should be included
    let block3 = driver.build_new_block_with_current_timestamp(None).await?;
    assert!(block3.includes(tx3.tx_hash()), "tx3 excluded from blocks 1-2 should be in block 3");

    Ok(())
}

/// Test that without the limit (default config), all transactions are included.
#[tokio::test]
async fn no_uncompressed_limit_includes_all_txs() -> eyre::Result<()> {
    let config = BuilderConfig::for_tests();
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;

    let calldata = Bytes::from(vec![0u8; 50_000]);

    let tx1 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(calldata.clone())
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(100)
        .send()
        .await?;

    let tx2 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(calldata.clone())
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(90)
        .send()
        .await?;

    let tx3 = driver
        .create_transaction()
        .with_to(Address::ZERO)
        .with_input(calldata)
        .with_gas_limit(600_000)
        .with_max_priority_fee_per_gas(80)
        .send()
        .await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    // Without the limit, all transactions should be included
    assert!(block.includes(tx1.tx_hash()), "tx1 should be included");
    assert!(block.includes(tx2.tx_hash()), "tx2 should be included");
    assert!(block.includes(tx3.tx_hash()), "tx3 should be included");

    Ok(())
}
