use crate::tests::{BlockTransactionsExt, LocalInstance};
use alloy_provider::Provider;
use macros::{if_flashblocks, if_standard, rb_test};
use reth_primitives_traits::constants::MEGAGAS;
/// This test ensures that the miner gas limit is respected
/// We will set the limit to 60,000 and see that the builder will not include any transactions
#[rb_test]
async fn miner_gas_limit(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;

    let call = driver
        .provider()
        .raw_request::<(u64,), bool>("miner_setGasLimit".into(), (60000,))
        .await?;
    assert!(call, "miner_setGasLimit should be executed successfully");

    let unfit_tx = driver.create_transaction().send().await?;
    let block = driver.build_new_block().await?;

    // tx should not be included because the gas limit is less than the transaction gas
    assert!(
        !block.includes(unfit_tx.tx_hash()),
        "transaction should not be included in the block"
    );

    Ok(())
}

/// This test ensures that block will fill up to the limit
/// Each transaction is 53000 gas
/// There is a deposit transaction for 24890 gas, and a builder transaction for 21600 gas
/// We will set our limit to 1Mgas and see that the builder includes 17 transactions
/// Total of 19 transactions including deposit + builder
/// In Flashblocks mode, there are 2 builder transactions, one at the beginning and one at the end of the block
/// So the total number of transactions in the block will be 20 for Flashblocks mode
#[rb_test]
async fn block_fill(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;

    let call = driver
        .provider()
        .raw_request::<(u64,), bool>("miner_setGasLimit".into(), (MEGAGAS,))
        .await?;
    assert!(call, "miner_setGasLimit should be executed successfully");

    let mut tx_hashes = Vec::new();
    for _ in 0..17 {
        let tx = driver
            .create_transaction()
            .with_gas_limit(53000)
            .with_max_priority_fee_per_gas(50)
            .send()
            .await?;
        tx_hashes.push(tx.tx_hash().clone());
    }
    let unfit_tx = driver
        .create_transaction()
        .with_gas_limit(53000)
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?;

    let block = driver.build_new_block().await?;

    for (i, tx_hash) in tx_hashes.iter().enumerate() {
        assert!(block.includes(tx_hash), "tx{} should be in block", i);
    }
    assert!(
        !block.includes(unfit_tx.tx_hash()),
        "unfit tx should not be in block"
    );

    if_standard! {
        assert_eq!(
            block.transactions.len(),
            19,
            "deposit + builder + 17 valid txs should be in the block"
        );
    }

    if_flashblocks! {
        assert_eq!(
            block.transactions.len(),
            20,
            "deposit + 2 builder + 17 valid txs should be in the block"
        );
    }

    Ok(())
}

/// This test ensures that the gasLimit can be reset to the default value
/// by setting it to 0
#[rb_test]
async fn reset_gas_limit(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;

    let call = driver
        .provider()
        .raw_request::<(u64,), bool>("miner_setGasLimit".into(), (60000,))
        .await?;
    assert!(call, "miner_setGasLimit should be executed successfully");

    let unfit_tx = driver.create_transaction().send().await?;
    let block = driver.build_new_block().await?;

    // tx should not be included because the gas limit is less than the transaction gas
    assert!(
        !block.includes(unfit_tx.tx_hash()),
        "transaction should not be included in the block"
    );

    let reset_call = driver
        .provider()
        .raw_request::<(u64,), bool>("miner_setGasLimit".into(), (0,))
        .await?;
    assert!(
        reset_call,
        "miner_setGasLimit should be executed successfully"
    );

    let _ = driver.build_new_block().await?;

    let fit_tx = driver.create_transaction().send().await?;
    let block = driver.build_new_block().await?;

    // tx should be included because the gas limit is reset to the default value
    assert!(
        block.includes(fit_tx.tx_hash()),
        "transaction should be in block"
    );

    Ok(())
}
