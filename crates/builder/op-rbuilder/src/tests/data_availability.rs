use alloy_provider::Provider;

use crate::tests::{
    BlockTransactionsExt, ChainDriverExt, TransactionBuilderExt, setup_test_instance,
};

/// This test ensures that the transaction size limit is respected.
/// We will set limit to 1 byte and see that the builder will not include any transactions.
#[tokio::test]
async fn tx_size_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    // Set (max_tx_da_size, max_block_da_size), with this case block won't fit any transaction
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (1, 0))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    let unfit_tx = driver.create_transaction().with_max_priority_fee_per_gas(50).send().await?;
    let block = driver.build_new_block().await?;

    // tx should not be included because we set the tx_size_limit to 1
    assert!(!block.includes(unfit_tx.tx_hash()), "transaction should not be included in the block");

    Ok(())
}

/// This test ensures that the block size limit is respected.
/// We will set limit to 1 byte and see that the builder will not include any transactions.
#[tokio::test]
async fn block_size_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    // Set block da size to be small, so it won't include tx
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 1))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    let (unfit_tx, block) = driver.build_new_block_with_valid_transaction().await?;

    // tx should not be included because we set the tx_size_limit to 1
    assert!(!block.includes(&unfit_tx), "transaction should not be included in the block");

    Ok(())
}

/// This test ensures that block will fill up to the limit.
/// Size of each transaction is 100
/// We will set limit to 3 txs and see that the builder will include 3 transactions.
#[tokio::test]
async fn block_fill() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    // Set DA limit so only 3 user transactions can fit (each tx is ~100 bytes)
    // Limit = 100 * 3 = 300 bytes, so 3 txs fit and the 4th won't
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 100 * 3))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    // Send transactions - some should fit within the DA limit and one should not
    let fit_tx_1 = driver.create_transaction().with_max_priority_fee_per_gas(50).send().await?;
    let fit_tx_2 = driver.create_transaction().with_max_priority_fee_per_gas(50).send().await?;
    let fit_tx_3 = driver.create_transaction().with_max_priority_fee_per_gas(50).send().await?;
    let unfit_tx_4 = driver.create_transaction().send().await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    // in flashblocks the DA quota is divided by the number of flashblocks
    // so we will include only some txs in the block because not all of them
    // will fit within DA quota / flashblocks count.
    assert!(block.includes(fit_tx_1.tx_hash()), "tx 1 should be in block");
    assert!(block.includes(fit_tx_2.tx_hash()), "tx 2 should be in block");
    assert!(block.includes(fit_tx_3.tx_hash()), "tx 3 should be in block");

    assert!(!block.includes(unfit_tx_4.tx_hash()), "tx 4 should not fit in block due to DA limit");
    assert!(
        driver.latest_full().await?.transactions.len() == 4,
        "deposit + 3 valid txs should be in the block"
    );

    Ok(())
}

/// This test ensures that the DA footprint limit (Jovian) is respected and the block fills
/// to the DA footprint limit. The DA footprint is calculated as:
/// total_da_bytes_used * da_footprint_gas_scalar (stored in blob_gas_used).
/// This must not exceed the block gas limit.
#[tokio::test]
async fn da_footprint_fills_to_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    // DA footprint scalar from JOVIAN_DATA is 400
    // Set a constrained gas limit so DA footprint becomes the limiting factor
    // With gas limit = 200_000 and scalar = 400:
    // Max DA bytes = 200_000 / 400 = 500 bytes
    let gas_limit = 400_000u64;
    let call = driver
        .provider()
        .raw_request::<(u64,), bool>("miner_setGasLimit".into(), (gas_limit,))
        .await?;
    assert!(call, "miner_setGasLimit should be executed successfully");

    // Set DA size limit to be permissive (not the constraint)
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 100_000))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    let mut tx_hashes = Vec::new();
    for _ in 0..12 {
        // Send more transactions to ensure some don't fit
        let tx = driver
            .create_transaction()
            .random_valid_transfer()
            .with_gas_limit(21000)
            .send()
            .await?;
        tx_hashes.push(*tx.tx_hash());
    }

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    // Verify that blob_gas_used (DA footprint) is set and respects limits
    assert!(block.header.blob_gas_used.is_some(), "blob_gas_used should be set in Jovian");

    let blob_gas = block.header.blob_gas_used.unwrap();

    // The DA footprint must not exceed the block gas limit
    assert!(
        blob_gas == gas_limit,
        "DA footprint (blob_gas_used={}) must not exceed block gas limit ({})",
        blob_gas,
        gas_limit
    );

    // Verify the block fills up to the DA footprint limit (flashblocks mode)
    // With more capacity now (no builder tx), more txs can fit
    for (i, tx_hash) in tx_hashes.iter().enumerate().take(9) {
        assert!(block.includes(tx_hash), "tx {} should be included in the block", i);
    }

    // Verify some txs don't fit due to DA footprint limit
    assert!(
        !block.includes(&tx_hashes[11]),
        "tx 11 should not fit in the block due to DA footprint limit"
    );

    Ok(())
}
