//! Smoke tests that a builder instance starts and produces blocks.

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::{collections::HashSet, sync::Arc};

use alloy_network::ReceiptResponse;
use alloy_primitives::{Bytes, TxHash};
#[cfg(target_os = "linux")]
use base_builder_core::test_utils::ExternalNode;
use base_builder_core::{
    BuilderConfig, StoragePrefetchPool,
    test_utils::{ChainDriver, Ipc, LocalInstance, TransactionBuilderExt, setup_test_instance},
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
                    ChainDriver::<Ipc>::BLOCK_TIME + Duration::from_millis(1500),
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
async fn chain_produces_big_tx() -> eyre::Result<()> {
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

/// Slots the heavy-writer contract writes on deployment and on every call: twice the prefetch
/// gate, so each such block prefetches the contract's storage trie.
const HEAVY_WRITER_SLOTS: u8 = 64;

/// Init code of a contract that sets slots `0..HEAVY_WRITER_SLOTS` to 1 and whose runtime code
/// sets every one of them to the current block number.
fn heavy_writer_init_code() -> Bytes {
    // NUMBER, PUSH1 slot, SSTORE for each slot, then STOP.
    let mut runtime: Vec<u8> =
        (0..HEAVY_WRITER_SLOTS).flat_map(|slot| [0x43, 0x60, slot, 0x55]).collect();
    runtime.push(0x00);
    // PUSH1 1, PUSH1 slot, SSTORE for each slot.
    let mut init: Vec<u8> =
        (0..HEAVY_WRITER_SLOTS).flat_map(|slot| [0x60, 0x01, 0x60, slot, 0x55]).collect();
    // CODECOPY(0, runtime offset, runtime length), then RETURN(0, runtime length).
    let copy_and_return_len = 15;
    let offset = u16::try_from(init.len() + copy_and_return_len).expect("init code fits");
    let length = u16::try_from(runtime.len()).expect("runtime code fits");
    init.push(0x61);
    init.extend(length.to_be_bytes());
    init.push(0x61);
    init.extend(offset.to_be_bytes());
    init.extend([0x60, 0x00, 0x39, 0x61]);
    init.extend(length.to_be_bytes());
    init.extend([0x60, 0x00, 0xf3]);
    init.extend(runtime);
    init.into()
}

/// Must hold: a builder with storage prefetch enabled stops the prefetch worker and release
/// threads in the node's graceful shutdown. A thread still reading MDBX when the process exits
/// races MDBX's process-wide teardown and aborts the process.
#[tokio::test]
async fn storage_prefetch_threads_stop_with_the_node() -> eyre::Result<()> {
    let pool = Arc::new(StoragePrefetchPool::spawn()?);
    let config =
        BuilderConfig { storage_prefetch: Some(Arc::clone(&pool)), ..BuilderConfig::for_tests() };
    let instance = LocalInstance::new(config).await?;
    let driver = instance.driver().await?;

    let deploy = driver
        .create_transaction()
        .with_create()
        .with_input(heavy_writer_init_code())
        .with_gas_limit(3_000_000)
        .send()
        .await?;
    driver.build_new_block().await?;
    let receipt = deploy.get_receipt().await?;
    assert!(receipt.status(), "the heavy writer must deploy");
    let contract = receipt.contract_address().expect("a deployment has a contract address");

    // The call rewrites every slot the parent state holds, so its build prefetches them.
    let call =
        driver.create_transaction().with_to(contract).with_gas_limit(1_000_000).send().await?;
    driver.build_new_block().await?;
    assert!(call.get_receipt().await?.status(), "the heavy write must succeed");

    drop(driver);
    drop(instance);
    assert_eq!(pool.live_threads(), 0, "a prefetch thread outlived the node's shutdown");
    Ok(())
}
