use super::framework::TestHarnessBuilder;
use alloy_provider::Provider;
use std::collections::HashSet;

/// This is a smoke test that ensures that transactions are included in blocks
/// and that the block generator is functioning correctly.
#[tokio::test]
async fn chain_produces_blocks() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("chain_produces_blocks")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    const SAMPLE_SIZE: usize = 10;

    // ensure that each block has at least two transactions when
    // no user transactions are sent.
    // the deposit transaction and the block generator's transaction
    for _ in 0..SAMPLE_SIZE {
        generator
            .generate_block()
            .await
            .expect("Failed to generate block");
        let transactions = harness.latest_block().await.transactions;
        assert!(
            transactions.len() == 2,
            "Block should have exactly two transactions"
        );
    }

    // ensure that transactions are included in blocks and each block has all the transactions
    // sent to it during its block time + the two mandatory transactions
    for _ in 0..SAMPLE_SIZE {
        let count = rand::random_range(1..8);
        let mut tx_hashes = HashSet::new();
        for _ in 0..count {
            let tx = harness
                .send_valid_transaction()
                .await
                .expect("Failed to send transaction");
            let tx_hash = *tx.tx_hash();
            tx_hashes.insert(tx_hash);
        }
        generator
            .generate_block()
            .await
            .expect("Failed to generate block");
        let transactions = harness.latest_block().await.transactions;

        assert!(
            transactions.len() == 2 + count,
            "Block should have {} transactions",
            2 + count
        );

        for tx_hash in tx_hashes {
            assert!(
                transactions.hashes().any(|hash| hash == *tx_hash),
                "Transaction {} should be included in the block",
                tx_hash
            );
        }
    }

    Ok(())
}

/// Ensures that payloads are generated correctly even when the builder is busy
/// with other requests, such as fcu or getPayload.
#[tokio::test]
async fn get_payload_close_to_fcu() -> eyre::Result<()> {
    let test_harness = TestHarnessBuilder::new("get_payload_close_to_fcu")
        .build()
        .await?;
    let mut block_generator = test_harness.block_generator().await?;

    // add some transactions to the pool so that the builder
    // is busy when we send the fcu/getPayload requests
    for _ in 0..10 {
        // Note, for this test it is okay if they are not valid
        let _ = test_harness.send_valid_transaction().await?;
    }

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        block_generator.submit_payload(None, 0, true),
    )
    .await;

    // ensure we didn't timeout
    let result = result.expect("Submit payload timed out");

    // ensure we got a payload
    assert!(result.is_ok(), "Failed to get payload: {:?}", result);

    Ok(())
}

/// This test validates that if we flood the builder with many transactions
/// and we request short block times, the builder can still eventually resolve all the transactions
#[tokio::test]
async fn transaction_flood_no_sleep() -> eyre::Result<()> {
    let test_harness = TestHarnessBuilder::new("transaction_flood_no_sleep")
        .build()
        .await?;

    let mut block_generator = test_harness.block_generator().await?;
    let provider = test_harness.provider()?;

    // Send 200 valid transactions to the builder
    // More than this and there is an issue with the RPC endpoint not being able to handle the load
    let mut transactions = vec![];
    for _ in 0..200 {
        let tx = test_harness.send_valid_transaction().await?;
        let tx_hash = *tx.tx_hash();
        transactions.push(tx_hash);
    }

    // After a 10 blocks all the transactions should be included in a block
    for _ in 0..10 {
        block_generator.submit_payload(None, 0, true).await.unwrap();
    }

    for tx in transactions {
        provider.get_transaction_receipt(tx).await?;
    }

    Ok(())
}
