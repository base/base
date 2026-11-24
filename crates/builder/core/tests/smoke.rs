//! Smoke tests that a builder instance starts and produces blocks.

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::collections::HashSet;

use alloy_primitives::TxHash;
#[cfg(target_os = "linux")]
use base_builder_core::test_utils::ExternalNode;
use base_builder_core::test_utils::{ChainDriver, Ipc, TransactionBuilderExt, setup_test_instance};
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

    for _ in 0..SAMPLE_SIZE {
        let block = driver.build_new_block_with_current_timestamp(None).await?;
        let transactions = block.transactions;

        assert_eq!(
            transactions.len(),
            1,
            "Empty blocks should have exactly one transaction (deposit)"
        );
    }

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
            loop {
                match driver.create_transaction().random_valid_transfer().send().await {
                    Ok(_) => {}
                    Err(e) if e.to_string().contains("txpool is full") => {
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

            for _ in 0..10 {
                let block = tokio::time::timeout(
                    ChainDriver::<Ipc>::BLOCK_TIME + Duration::from_millis(1500),
                    driver.build_new_block_with_current_timestamp(None),
                )
                .await
                .expect("Timeout while waiting for block production")
                .expect("Failed to produce block under load");

                info!(block = ?block, "Produced a block under load");

                yield_now().await;
            }

            done.store(true, Ordering::Relaxed);
            info!("All blocks produced under load");

            Ok::<(), eyre::Error>(())
        }
    );

    populate.unwrap();

    assert!(produce.is_ok(), "Failed to produce block under load");

    Ok(())
}

#[tokio::test]
async fn test_no_tx_pool() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    let _ = driver.build_new_block().await?;

    let _ = driver.build_new_block_with_no_tx_pool().await?;

    Ok(())
}

#[tokio::test]
async fn chain_produces_big_tx() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;

    #[cfg(target_os = "linux")]
    let driver = driver.with_validation_node(ExternalNode::reth().await?).await?;

    let tx = driver
        .create_transaction()
        .random_big_transaction()
        .send()
        .await
        .expect("Failed to send transaction");

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    let txs = block.transactions;

    let inclusion_result = txs.hashes().find(|hash| hash == tx.tx_hash());
    assert!(inclusion_result.is_some());

    assert_eq!(txs.len(), 2, "Should have 2 transactions (deposit + big tx)");

    Ok(())
}
