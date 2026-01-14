use std::time::Duration;

use alloy_eips::{BlockNumberOrTag::Latest, Encodable2718, eip1559::MIN_PROTOCOL_BASE_FEE};
use alloy_primitives::bytes;

use crate::tests::{BlockTransactionsExt, setup_test_instance};

#[tokio::test]
async fn jovian_block_parameters_set() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let tx_one = driver.create_transaction().send().await?;
    let tx_two = driver.create_transaction().send().await?;
    let block = driver.build_new_block().await?;

    assert!(block.includes(tx_one.tx_hash()));
    assert!(block.includes(tx_two.tx_hash()));

    assert!(block.header.excess_blob_gas.is_some());

    assert!(block.header.blob_gas_used.is_some());

    // Two user transactions + two builder transactions, all minimum size (flashblocks mode)
    assert_eq!(block.header.blob_gas_used.unwrap(), 160_000);

    // Version byte
    assert_eq!(block.header.extra_data.slice(0..1), bytes!("0x01"));

    // Min Base Fee of zero by default
    assert_eq!(block.header.extra_data.slice(9..=16), bytes!("0x0000000000000000"),);

    Ok(())
}

#[tokio::test]
async fn jovian_no_tx_pool_sync() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let block =
        driver.build_new_block_with_txs_timestamp(vec![], Some(true), None, None, Some(0)).await?;

    // Deposit transaction only (flashblocks mode)
    assert_eq!(block.transactions.len(), 1);
    assert_eq!(block.header.blob_gas_used, Some(0));

    let tx = driver.create_transaction().build().await;
    let block = driver
        .build_new_block_with_txs_timestamp(
            vec![tx.encoded_2718().into()],
            Some(true),
            None,
            None,
            Some(0),
        )
        .await?;

    // Deposit transaction + user transaction (flashblocks mode)
    assert_eq!(block.transactions.len(), 2);
    assert_eq!(block.header.blob_gas_used, Some(40_000));

    Ok(())
}

#[tokio::test]
async fn jovian_minimum_base_fee() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let genesis = driver.get_block(Latest).await?.expect("must have genesis block");

    assert_eq!(genesis.header.base_fee_per_gas, Some(1));

    let min_base_fee = Some(MIN_PROTOCOL_BASE_FEE * 2);

    let block_timestamp = Duration::from_secs(genesis.header.timestamp) + Duration::from_secs(1);
    let block_one = driver
        .build_new_block_with_txs_timestamp(vec![], None, Some(block_timestamp), None, min_base_fee)
        .await?;

    assert_eq!(block_one.header.extra_data.slice(9..=16), bytes!("0x000000000000000E"),);

    let overpriced_tx = driver
        .create_transaction()
        .with_max_fee_per_gas(MIN_PROTOCOL_BASE_FEE as u128 * 4)
        .send()
        .await?;
    let underpriced_tx = driver
        .create_transaction()
        .with_max_fee_per_gas(MIN_PROTOCOL_BASE_FEE as u128)
        .send()
        .await?;

    let block_timestamp = Duration::from_secs(block_one.header.timestamp) + Duration::from_secs(1);
    let block_two = driver
        .build_new_block_with_txs_timestamp(vec![], None, Some(block_timestamp), None, min_base_fee)
        .await?;

    assert_eq!(block_two.header.extra_data.slice(9..=16), bytes!("0x000000000000000E"),);

    assert!(block_two.includes(overpriced_tx.tx_hash()));
    assert!(!block_two.includes(underpriced_tx.tx_hash()));

    Ok(())
}

#[tokio::test]
async fn jovian_minimum_fee_must_be_set() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let genesis = driver.get_block(Latest).await?.expect("must have genesis block");
    let block_timestamp = Duration::from_secs(genesis.header.timestamp) + Duration::from_secs(1);
    let response = driver
        .build_new_block_with_txs_timestamp(vec![], None, Some(block_timestamp), None, None)
        .await;
    assert!(response.is_err());
    Ok(())
}
