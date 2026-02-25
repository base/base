#![allow(missing_docs)]

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::collections::HashSet;

use alloy_primitives::TxHash;
#[cfg(target_os = "linux")]
use base_builder_core::test_utils::ExternalNode;
use base_builder_core::{
    BuilderConfig,
    test_utils::{
        TransactionBuilderExt, setup_test_instance, setup_test_instance_with_builder_config,
    },
};
use tokio::{join, task::yield_now};
use tracing::info;

/// This is a smoke test that ensures that transactions are included in blocks
/// and that the block generator is functioning correctly.
///
/// Generated blocks are also validated against an external op-reth node to
/// ensure their correctness.
#[tokio::test]
async fn chain_produces_blocks() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    #[cfg(target_os = "linux")]
    let driver = driver.with_validation_node(ExternalNode::reth().await?).await?;

    const SAMPLE_SIZE: usize = 10;

    // ensure that each block has the deposit transaction when
    // no user transactions are sent.
    for _ in 0..SAMPLE_SIZE {
        let block = driver.build_new_block_with_current_timestamp(None).await?;
        let transactions = block.transactions;

        // Only the deposit transaction should be present
        assert_eq!(
            transactions.len(),
            1,
            "Empty blocks should have exactly one transaction (deposit)"
        );
    }

    // ensure that transactions are included in blocks and each block has all the transactions
    // sent to it during its block time plus the deposit transaction
    for _ in 0..SAMPLE_SIZE {
        let count = rand::random_range(1..8);
        let mut tx_hashes = HashSet::<TxHash>::default();

        for _ in 0..count {
            let tx = driver
                .create_transaction()
                .random_valid_transfer()
                .send()
                .await
                .expect("Failed to send transaction");
            tx_hashes.insert(*tx.tx_hash());
        }

        let block = driver.build_new_block_with_current_timestamp(None).await?;

        let txs = block.transactions;

        // Each block contains the deposit transaction plus user transactions
        assert_eq!(txs.len(), 1 + count, "Block should have {} transactions", 1 + count);

        for tx_hash in tx_hashes {
            assert!(
                txs.hashes().any(|hash| hash == tx_hash),
                "Transaction {tx_hash} should be included in the block"
            );
        }
    }
    Ok(())
}

/// Ensures that payloads are generated correctly even when the builder is busy
/// with other requests, such as fcu or getPayload.
#[tokio::test(flavor = "multi_thread")]
async fn produces_blocks_under_load_within_deadline() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?.with_gas_limit(1_000_000);

    let done = AtomicBool::new(false);

    let (populate, produce) = join!(
        async {
            // Keep the builder busy with new transactions.
            loop {
                match driver.create_transaction().random_valid_transfer().send().await {
                    Ok(_) => {}
                    Err(e) if e.to_string().contains("txpool is full") => {
                        // If the txpool is full, give it a short break
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => return Err(e),
                };

                if done.load(Ordering::Relaxed) {
                    break;
                }

                yield_now().await;
            }
            Ok::<(), eyre::Error>(())
        },
        async {
            // Wait for a short time to allow the transaction population to start
            // and fill up the txpool.
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Now, start producing blocks under load.
            for _ in 0..10 {
                // Ensure that the builder can still produce blocks under
                // heavy load of incoming transactions.
                let block = tokio::time::timeout(
                    rbuilder.builder_config().block_time + Duration::from_millis(1500),
                    driver.build_new_block_with_current_timestamp(None),
                )
                .await
                .expect("Timeout while waiting for block production")
                .expect("Failed to produce block under load");

                info!(block = ?block, "Produced a block under load");

                yield_now().await;
            }

            // we're happy with one block produced under load
            // set the done flag to true to stop the transaction population
            done.store(true, Ordering::Relaxed);
            info!("All blocks produced under load");

            Ok::<(), eyre::Error>(())
        }
    );

    populate.unwrap();

    //assert!(populate.is_ok(), "Failed to populate transactions");
    assert!(produce.is_ok(), "Failed to produce block under load");

    Ok(())
}

#[tokio::test]
async fn test_no_tx_pool() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    // make sure we can build a couple of blocks first
    let _ = driver.build_new_block().await?;

    // now lets try to build a block with no transactions
    let _ = driver.build_new_block_with_no_tx_pool().await?;

    Ok(())
}

#[tokio::test]
async fn chain_produces_big_tx_with_gas_limit() -> eyre::Result<()> {
    let config = BuilderConfig::for_tests().with_max_gas_per_txn(Some(25000));
    let rbuilder = setup_test_instance_with_builder_config(config).await?;
    let driver = rbuilder.driver().await?;

    #[cfg(target_os = "linux")]
    let driver = driver.with_validation_node(ExternalNode::reth().await?).await?;

    // insert valid txn under limit
    let tx = driver
        .create_transaction()
        .random_valid_transfer()
        .send()
        .await
        .expect("Failed to send transaction");

    // insert txn with gas usage above limit
    let tx_high_gas = driver
        .create_transaction()
        .random_big_transaction()
        .send()
        .await
        .expect("Failed to send transaction");

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    let txs = block.transactions;

    // deposit + valid user tx (high gas tx excluded due to limit)
    assert_eq!(txs.len(), 2, "Should have 2 transactions (deposit + valid user tx)");

    // assert we included the tx with gas under limit
    let inclusion_result = txs.hashes().find(|hash| hash == tx.tx_hash());
    assert!(inclusion_result.is_some());

    // assert we do not include the tx with gas above limit
    let exclusion_result = txs.hashes().find(|hash| hash == tx_high_gas.tx_hash());
    assert!(exclusion_result.is_none());

    Ok(())
}

#[tokio::test]
async fn chain_produces_big_tx_without_gas_limit() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    #[cfg(target_os = "linux")]
    let driver = driver.with_validation_node(ExternalNode::reth().await?).await?;

    // insert txn with gas usage but there is no limit
    let tx = driver
        .create_transaction()
        .random_big_transaction()
        .send()
        .await
        .expect("Failed to send transaction");

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    let txs = block.transactions;

    // assert we included the tx
    let inclusion_result = txs.hashes().find(|hash| hash == tx.tx_hash());
    assert!(inclusion_result.is_some());

    // deposit + big tx (no gas limit to exclude it)
    assert_eq!(txs.len(), 2, "Should have 2 transactions (deposit + big tx)");

    Ok(())
}
