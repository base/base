#![allow(missing_docs)]

use std::{sync::Arc, time::Duration};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::TxHash;
use alloy_provider::Provider;
use base_builder_core::test_utils::{
    ChainDriver, ChainDriverExt, ExternalTransactionPool, LocalInstance, ONE_ETH, PrivateKeySigner,
    Protocol, TransactionBuilderExt, setup_test_instance,
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
    insert_bundle_transaction_with_nonce(
        pool,
        driver,
        signer,
        None,
        target_block_number,
        min_timestamp,
        max_timestamp,
    )
    .await
}

async fn insert_bundle_transaction_with_nonce<P: Protocol>(
    pool: &Arc<dyn ExternalTransactionPool>,
    driver: &ChainDriver<P>,
    signer: &PrivateKeySigner,
    nonce: Option<u64>,
    target_block_number: Option<u64>,
    min_timestamp: Option<u64>,
    max_timestamp: Option<u64>,
) -> eyre::Result<TxHash> {
    let mut builder = driver.create_transaction().with_signer(signer).random_valid_transfer();
    if let Some(n) = nonce {
        builder = builder.with_nonce(n);
    }
    let recovered = builder.build().await;
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

async fn wait_for_pool_visibility(
    rbuilder: &LocalInstance,
    tx_hashes: &[TxHash],
) -> eyre::Result<()> {
    let wait_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tx_hashes.iter().all(|hash| rbuilder.pool().exists(*hash)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;

    if wait_result.is_err() {
        let statuses = tx_hashes
            .iter()
            .map(|hash| {
                let status = rbuilder.pool().tx_status(*hash);
                let history = rbuilder.pool().history(*hash);
                format!("hash={hash:?} status={status:?} history={history:?}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(eyre::eyre!("timed out waiting for bundle txs to enter the pool: {statuses}"));
    }

    Ok(())
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
    wait_for_pool_visibility(&rbuilder, &[tx_hash]).await?;

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
    wait_for_pool_visibility(&rbuilder, &[tx_hash]).await?;

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
    wait_for_pool_visibility(&rbuilder, &[tx_hash]).await?;

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
    wait_for_pool_visibility(&rbuilder, &[tx_hash]).await?;

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

#[tokio::test]
async fn two_valid_bundles_from_same_sender_are_both_included() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let latest = driver.latest().await?;
    let target_block = latest.header.number + 1;
    let pool = rbuilder.pool_handle();
    let base_nonce = driver.provider().get_transaction_count(signer.address()).pending().await?;

    let tx_hash_0 = insert_bundle_transaction_with_nonce(
        &pool,
        &driver,
        &signer,
        Some(base_nonce),
        Some(target_block),
        None,
        None,
    )
    .await?;

    let tx_hash_1 = insert_bundle_transaction_with_nonce(
        &pool,
        &driver,
        &signer,
        Some(base_nonce + 1),
        Some(target_block),
        None,
        None,
    )
    .await?;
    wait_for_pool_visibility(&rbuilder, &[tx_hash_0, tx_hash_1]).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert_eq!(block.header.number, target_block);
    assert!(block.transactions.hashes().any(|hash| hash == tx_hash_0));
    assert!(block.transactions.hashes().any(|hash| hash == tx_hash_1));

    Ok(())
}

#[tokio::test]
async fn expired_bundle_excluded_while_valid_bundle_included_for_same_sender() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let signer = driver.fund_accounts(1, ONE_ETH).await?.remove(0);
    let latest = driver.latest().await?;
    let target_block = latest.header.number + 1;
    let pool = rbuilder.pool_handle();
    let base_nonce = driver.provider().get_transaction_count(signer.address()).pending().await?;

    let expired_timestamp = 1u64;

    let valid_tx_hash = insert_bundle_transaction_with_nonce(
        &pool,
        &driver,
        &signer,
        Some(base_nonce),
        Some(target_block),
        None,
        None,
    )
    .await?;

    let expired_tx_hash = insert_bundle_transaction_with_nonce(
        &pool,
        &driver,
        &signer,
        Some(base_nonce + 1),
        Some(target_block),
        None,
        Some(expired_timestamp),
    )
    .await?;
    // Only the valid tx is required to remain in the pool. The expired one can
    // be evicted before block build due to bundle timestamp constraints.
    wait_for_pool_visibility(&rbuilder, &[valid_tx_hash]).await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;

    assert_eq!(block.header.number, target_block);
    assert!(
        block.transactions.hashes().any(|hash| hash == valid_tx_hash),
        "valid bundle tx should be included"
    );
    assert!(
        block.transactions.hashes().all(|hash| hash != expired_tx_hash),
        "expired bundle tx should be excluded"
    );

    Ok(())
}
