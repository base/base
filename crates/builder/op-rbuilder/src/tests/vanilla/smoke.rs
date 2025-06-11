use crate::tests::{LocalInstance, TransactionBuilderExt};
use alloy_primitives::TxHash;
use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::collections::HashSet;
use tokio::{join, task::yield_now};
use tracing::info;

/// This is a smoke test that ensures that transactions are included in blocks
/// and that the block generator is functioning correctly.
///
/// Generated blocks are also validated against an external op-reth node to
/// ensure their correctness.
#[tokio::test]
async fn chain_produces_blocks() -> eyre::Result<()> {
    let rbuilder = LocalInstance::standard().await?;
    let driver = rbuilder.driver().await?;

    #[cfg(target_os = "linux")]
    let driver = driver
        .with_validation_node(crate::tests::ExternalNode::reth().await?)
        .await?;

    const SAMPLE_SIZE: usize = 10;

    // ensure that each block has at least two transactions when
    // no user transactions are sent.
    // the deposit transaction and the block generator's transaction
    for _ in 0..SAMPLE_SIZE {
        let block = driver.build_new_block().await?;
        let transactions = block.transactions;

        assert_eq!(
            transactions.len(),
            2,
            "Empty blocks should have exactly two transactions"
        );
    }

    // ensure that transactions are included in blocks and each block has all the transactions
    // sent to it during its block time + the two mandatory transactions
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

        let block = driver.build_new_block().await?;

        let txs = block.transactions;

        assert_eq!(
            txs.len(),
            2 + count,
            "Block should have {} transactions",
            2 + count
        );

        for tx_hash in tx_hashes {
            assert!(
                txs.hashes().any(|hash| hash == tx_hash),
                "Transaction {} should be included in the block",
                tx_hash
            );
        }
    }
    Ok(())
}

/// Ensures that payloads are generated correctly even when the builder is busy
/// with other requests, such as fcu or getPayload.
#[tokio::test(flavor = "multi_thread")]
async fn produces_blocks_under_load_within_deadline() -> eyre::Result<()> {
    let rbuilder = LocalInstance::standard().await?;
    let driver = rbuilder.driver().await?.with_gas_limit(10_00_000);

    let done = AtomicBool::new(false);

    let (populate, produce) = join!(
        async {
            // Keep the builder busy with new transactions.
            loop {
                match driver
                    .create_transaction()
                    .random_valid_transfer()
                    .send()
                    .await
                {
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
                    Duration::from_secs(rbuilder.args().chain_block_time)
                        + Duration::from_millis(500),
                    driver.build_new_block(),
                )
                .await
                .expect("Timeout while waiting for block production")
                .expect("Failed to produce block under load");

                info!("Produced a block under load: {block:#?}");

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
