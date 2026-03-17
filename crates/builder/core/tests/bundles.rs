#![allow(missing_docs)]

use std::sync::Arc;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::TxHash;
use base_builder_core::test_utils::{
    ChainDriver, ChainDriverExt, ExternalTransactionPool, ONE_ETH, PrivateKeySigner, Protocol,
    TransactionBuilderExt, setup_test_instance,
};
use base_txpool::{BasePooledTransaction, unix_time_millis};

async fn insert_bundle_transaction<P: Protocol>(
    pool: &Arc<dyn ExternalTransactionPool>,
    driver: &ChainDriver<P>,
    signer: &PrivateKeySigner,
    target_block_number: Option<u64>,
    min_timestamp: Option<u64>,
    max_timestamp: Option<u64>,
) -> eyre::Result<TxHash> {
    let recovered =
        driver.create_transaction().with_signer(signer).random_valid_transfer().build().await;
    let tx_hash = TxHash::from(*recovered.tx_hash());
    let encoded_len = recovered.encode_2718_len();
    let pool_tx = BasePooledTransaction::new(recovered, encoded_len).with_bundle_metadata(
        target_block_number,
        min_timestamp,
        max_timestamp,
    );

    pool.add_external_transaction(pool_tx).await?;

    Ok(tx_hash)
}

#[tokio::test]
async fn bundle_tx_targeting_current_block_is_included() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let latest = driver.latest().await?;
    let target_block = latest.header.number + 1;
    let pool = rbuilder.pool_handle();

    let tx_hash =
        insert_bundle_transaction(&pool, &driver, &signer, Some(target_block), None, None).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert_eq!(block.header.number, target_block);
    assert!(block.transactions.hashes().any(|hash| hash == tx_hash));

    Ok(())
}

#[tokio::test]
async fn bundle_tx_targeting_different_block_is_excluded() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let latest = driver.latest().await?;
    let target_block = latest.header.number + 2;
    let pool = rbuilder.pool_handle();

    let tx_hash =
        insert_bundle_transaction(&pool, &driver, &signer, Some(target_block), None, None).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert!(block.transactions.hashes().all(|hash| hash != tx_hash));

    Ok(())
}

#[tokio::test]
async fn expired_bundle_tx_is_excluded() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let pool = rbuilder.pool_handle();
    let max_timestamp = unix_time_millis().saturating_sub(1_000) as u64;

    let tx_hash =
        insert_bundle_transaction(&pool, &driver, &signer, None, None, Some(max_timestamp)).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert!(block.transactions.hashes().all(|hash| hash != tx_hash));

    Ok(())
}

#[tokio::test]
async fn future_bundle_tx_is_deferred_but_not_invalidated() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let pool = rbuilder.pool_handle();
    let min_timestamp = unix_time_millis().saturating_add(60_000) as u64;

    let tx_hash =
        insert_bundle_transaction(&pool, &driver, &signer, None, Some(min_timestamp), None).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert!(block.transactions.hashes().all(|hash| hash != tx_hash));
    assert!(rbuilder.pool().exists(tx_hash));

    Ok(())
}

#[tokio::test]
async fn normal_transaction_is_unaffected_by_bundle_checks() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);

    let tx =
        driver.create_transaction().with_signer(&signer).random_valid_transfer().send().await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    let tx_hash = *tx.tx_hash();

    assert!(block.transactions.hashes().any(|hash| hash == tx_hash));
    Ok(())
}
