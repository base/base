use crate::tests::TestHarnessBuilder;

/// This test ensures that the transactions that get reverted an not included in the block
/// are emitted as a log on the builder.
#[tokio::test]
async fn monitor_transaction_drops() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("monitor_transaction_drops")
        .with_revert_protection()
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    // send 10 reverting transactions
    let mut pending_txn = Vec::new();
    for _ in 0..10 {
        let txn = harness.send_revert_transaction().await?;
        pending_txn.push(txn);
    }

    // generate 10 blocks
    for _ in 0..10 {
        generator.generate_block().await?;
        let latest_block = harness.latest_block().await;

        // blocks should only include two transactions (deposit + builder)
        assert_eq!(latest_block.transactions.len(), 2);
    }

    // check that the builder emitted logs for the reverted transactions
    // with the monitoring logic
    // TODO: this is not ideal, lets find a different way to detect this
    // Each time a transaction is dropped, it emits a log like this
    // 'Transaction event received target="monitoring" tx_hash="<tx_hash>" kind="discarded"'
    let builder_logs = std::fs::read_to_string(harness.builder_log_path())?;

    for txn in pending_txn {
        let txn_log = format!(
            "Transaction event received target=\"monitoring\" tx_hash=\"{}\" kind=\"discarded\"",
            txn.tx_hash()
        );

        assert!(builder_logs.contains(txn_log.as_str()));
    }

    Ok(())
}

#[tokio::test]
async fn revert_protection_disabled() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection_disabled")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    for _ in 0..10 {
        let valid_tx = harness.send_valid_transaction().await?;
        let reverting_tx = harness.send_revert_transaction().await?;
        let block_generated = generator.generate_block().await?;

        assert!(block_generated.includes(*valid_tx.tx_hash()));
        assert!(block_generated.includes(*reverting_tx.tx_hash()));
    }

    Ok(())
}

#[tokio::test]
async fn revert_protection() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection")
        .with_revert_protection()
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    for _ in 0..10 {
        let valid_tx = harness.send_valid_transaction().await?;
        let reverting_tx = harness.send_revert_transaction().await?;
        let block_generated = generator.generate_block().await?;

        assert!(block_generated.includes(*valid_tx.tx_hash()));
        assert!(block_generated.not_includes(*reverting_tx.tx_hash()));
    }

    Ok(())
}
