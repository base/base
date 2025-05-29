use alloy_provider::{PendingTransactionBuilder, Provider};
use op_alloy_network::Optimism;

use crate::{
    primitives::bundle::MAX_BLOCK_RANGE_BLOCKS,
    tests::{BundleOpts, TestHarness, TestHarnessBuilder},
};

/// This test ensures that the transactions that get reverted and not included in the block,
/// are eventually dropped from the pool once their block range is reached.
/// This test creates N transactions with different block ranges.
#[tokio::test]
async fn revert_protection_monitor_transaction_gc() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection_monitor_transaction_gc")
        .with_revert_protection()
        .with_namespaces("eth,web3,txpool")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    // send 10 bundles with different block ranges
    let mut pending_txn = Vec::new();
    for i in 1..=10 {
        let txn = harness
            .create_transaction()
            .with_revert()
            .with_bundle(BundleOpts {
                block_number_max: Some(i),
            })
            .send()
            .await?;
        pending_txn.push(txn);
    }

    // generate 10 blocks
    for i in 0..10 {
        let generated_block = generator.generate_block().await?;

        // blocks should only include two transactions (deposit + builder)
        assert_eq!(generated_block.block.transactions.len(), 2);

        // since we created the 10 transactions with increasing block ranges, as we generate blocks
        // one transaction will be gc on each block.
        // transactions from [0, i] should be dropped, transactions from [i+1, 10] should be queued
        for j in 0..=i {
            let status = harness.check_tx_in_pool(*pending_txn[j].tx_hash()).await?;
            assert!(status.is_dropped());
        }
        for j in i + 1..10 {
            let status = harness.check_tx_in_pool(*pending_txn[j].tx_hash()).await?;
            assert!(status.is_queued());
        }
    }

    Ok(())
}

/// If revert protection is disabled, the transactions that revert are included in the block.
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

/// If revert protection is disabled, it should not be possible to send a revert bundle
/// since the revert RPC endpoint is not available.
#[tokio::test]
async fn revert_protection_disabled_bundle_endpoint_error() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection_disabled_bundle_endpoint_error")
        .build()
        .await?;

    let res = harness
        .create_transaction()
        .with_bundle(BundleOpts::default())
        .send()
        .await;

    assert!(
        res.is_err(),
        "Expected error because method is not available"
    );
    Ok(())
}

/// Test the behaviour of the revert protection bundle, if the bundle **does not** revert
/// the transaction is included in the block. If the bundle reverts, the transaction
/// is not included in the block and tried again for the next bundle range blocks
/// when it will be dropped from the pool.
#[tokio::test]
async fn revert_protection_bundle() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection_bundle")
        .with_revert_protection()
        .with_namespaces("eth,web3,txpool")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?; // Block 1

    // Test 1: Bundle does not revert
    let valid_bundle = harness
        .create_transaction()
        .with_bundle(BundleOpts::default())
        .send()
        .await?;

    let block_generated = generator.generate_block().await?; // Block 2
    assert!(block_generated.includes(*valid_bundle.tx_hash()));

    let bundle_opts = BundleOpts {
        block_number_max: Some(4),
    };

    let reverted_bundle = harness
        .create_transaction()
        .with_revert()
        .with_bundle(bundle_opts)
        .send()
        .await?;

    // Test 2: Bundle reverts. It is not included in the block
    let block_generated = generator.generate_block().await?; // Block 3
    assert!(block_generated.not_includes(*reverted_bundle.tx_hash()));

    // After the block the transaction is still pending in the pool
    assert!(harness
        .check_tx_in_pool(*reverted_bundle.tx_hash())
        .await?
        .is_pending());

    // Test 3: Chain progresses beyond the bundle range. The transaction is dropped from the pool
    generator.generate_block().await?; // Block 4
    assert!(harness
        .check_tx_in_pool(*reverted_bundle.tx_hash())
        .await?
        .is_pending());

    generator.generate_block().await?; // Block 5
    assert!(harness
        .check_tx_in_pool(*reverted_bundle.tx_hash())
        .await?
        .is_dropped());

    Ok(())
}

/// Test the range limits for the revert protection bundle.
#[tokio::test]
async fn revert_protection_bundle_range_limits() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("revert_protection_bundle_range_limits")
        .with_revert_protection()
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    // Advance two blocks and try to send a bundle with max block = 1
    generator.generate_block().await?; // Block 1
    generator.generate_block().await?; // Block 2

    async fn send_bundle(
        harness: &TestHarness,
        block_number_max: u64,
    ) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        harness
            .create_transaction()
            .with_bundle(BundleOpts {
                block_number_max: Some(block_number_max),
            })
            .send()
            .await
    }

    // Max block cannot be a past block
    assert!(send_bundle(&harness, 1).await.is_err());

    // Bundles are valid if their max block in in between the current block and the max block range
    let next_valid_block = 3;

    for i in next_valid_block..next_valid_block + MAX_BLOCK_RANGE_BLOCKS {
        assert!(send_bundle(&harness, i).await.is_ok());
    }

    // A bundle with a block out of range is invalid
    assert!(
        send_bundle(&harness, next_valid_block + MAX_BLOCK_RANGE_BLOCKS + 1)
            .await
            .is_err()
    );

    Ok(())
}

/// If a transaction reverts and was sent as a normal transaction through the eth_sendRawTransaction
/// bundle, the transaction should be included in the block.
/// This behaviour is the same as the 'revert_protection_disabled' test.
#[tokio::test]
async fn revert_protection_allow_reverted_transactions_without_bundle() -> eyre::Result<()> {
    let harness =
        TestHarnessBuilder::new("revert_protection_allow_reverted_transactions_without_bundle")
            .with_revert_protection()
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

/// If a transaction reverts and gets dropped it, the eth_getTransactionReceipt should return
/// an error message that it was dropped.
#[tokio::test]
async fn revert_protection_check_transaction_receipt_status_message() -> eyre::Result<()> {
    let harness =
        TestHarnessBuilder::new("revert_protection_check_transaction_receipt_status_message")
            .with_revert_protection()
            .build()
            .await?;

    let provider = harness.provider()?;
    let mut generator = harness.block_generator().await?;

    let reverting_tx = harness
        .create_transaction()
        .with_revert()
        .with_bundle(BundleOpts {
            block_number_max: Some(3),
        })
        .send()
        .await?;
    let tx_hash = reverting_tx.tx_hash();

    let _ = generator.generate_block().await?;
    let receipt = provider.get_transaction_receipt(*tx_hash).await?;
    assert!(receipt.is_none());

    let _ = generator.generate_block().await?;
    let receipt = provider.get_transaction_receipt(*tx_hash).await?;
    assert!(receipt.is_none());

    // Dropped
    let _ = generator.generate_block().await?;
    let receipt = provider.get_transaction_receipt(*tx_hash).await;
    assert!(receipt.is_err());

    Ok(())
}
