use crate::tests::{BlockTransactionsExt, ChainDriverExt, LocalInstance};
use alloy_provider::Provider;

/// This test ensures that the transaction size limit is respected.
/// We will set limit to 1 byte and see that the builder will not include any transactions.
#[tokio::test]
async fn tx_size_limit() -> eyre::Result<()> {
    let rbuilder = LocalInstance::standard().await?;
    let driver = rbuilder.driver().await?;

    // Set (max_tx_da_size, max_block_da_size), with this case block won't fit any transaction
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (1, 0))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    let unfit_tx = driver
        .create_transaction()
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?;
    let block = driver.build_new_block().await?;

    // tx should not be included because we set the tx_size_limit to 1
    assert!(
        !block.includes(unfit_tx.tx_hash()),
        "transaction should not be included in the block"
    );

    Ok(())
}

/// This test ensures that the block size limit is respected.
/// We will set limit to 1 byte and see that the builder will not include any transactions.
#[tokio::test]
async fn block_size_limit() -> eyre::Result<()> {
    let rbuilder = LocalInstance::standard().await?;
    let driver = rbuilder.driver().await?;

    // Set block da size to be small, so it won't include tx
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 1))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    let (unfit_tx, block) = driver.build_new_block_with_valid_transaction().await?;

    // tx should not be included because we set the tx_size_limit to 1
    assert!(
        !block.includes(&unfit_tx),
        "transaction should not be included in the block"
    );

    Ok(())
}

/// This test ensures that block will fill up to the limit.
/// Size of each transaction is 100000000
/// We will set limit to 3 txs and see that the builder will include 3 transactions.
/// We should not forget about builder transaction so we will spawn only 2 regular txs.
#[tokio::test]
async fn block_fill() -> eyre::Result<()> {
    let rbuilder = LocalInstance::standard().await?;
    let driver = rbuilder.driver().await?;

    // Set block big enough so it could fit 3 transactions without tx size limit
    let call = driver
        .provider()
        .raw_request::<(i32, i32), bool>("miner_setMaxDASize".into(), (0, 100 * 3))
        .await?;
    assert!(call, "miner_setMaxDASize should be executed successfully");

    // We already have 2 so we will spawn one more to check that it won't be included (it won't fit
    // because of builder tx)
    let fit_tx_1 = driver
        .create_transaction()
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?;
    let fit_tx_2 = driver
        .create_transaction()
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?;
    let unfit_tx_3 = driver.create_transaction().send().await?;

    let block = driver.build_new_block().await?;
    // Now the first 2 txs will fit into the block
    assert!(block.includes(fit_tx_1.tx_hash()), "tx should be in block");
    assert!(block.includes(fit_tx_2.tx_hash()), "tx should be in block");
    assert!(
        !block.includes(unfit_tx_3.tx_hash()),
        "unfit tx should not be in block"
    );
    assert!(
        driver.latest_full().await?.transactions.len() == 4,
        "builder + deposit + 2 valid txs should be in the block"
    );

    Ok(())
}
