use crate::tests::{framework::TestHarnessBuilder, ONE_ETH};
use alloy_provider::ext::TxPoolApi;

/// This test ensures that pending pool custom limit is respected and priority tx would be included even when pool if full.
#[tokio::test]
async fn pending_pool_limit() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("pending_pool_limit")
        .with_namespaces("txpool,eth,debug,admin")
        .with_extra_params("--txpool.pending-max-count 50")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;

    // Send 50 txs from different addrs
    let accounts = generator.create_funded_accounts(2, ONE_ETH).await?;
    let acc_no_priority = accounts.first().unwrap();
    let acc_with_priority = accounts.last().unwrap();

    for _ in 0..50 {
        let _ = harness
            .create_transaction()
            .with_signer(*acc_no_priority)
            .send()
            .await?;
    }

    let pool = harness
        .provider()
        .expect("provider not available")
        .txpool_status()
        .await?;
    assert_eq!(
        pool.pending, 50,
        "Pending pool must contain at max 50 txs {:?}",
        pool
    );

    // Send 10 txs that should be included in the block
    let mut txs = Vec::new();
    for _ in 0..10 {
        let tx = harness
            .create_transaction()
            .with_signer(*acc_with_priority)
            .with_max_priority_fee_per_gas(10)
            .send()
            .await?;
        txs.push(*tx.tx_hash());
    }

    let pool = harness
        .provider()
        .expect("provider not available")
        .txpool_status()
        .await?;
    assert_eq!(
        pool.pending, 50,
        "Pending pool must contain at max 50 txs {:?}",
        pool
    );

    // After we try building block our reverting tx would be removed and other tx will move to queue pool
    let block = generator.generate_block().await?;

    // Ensure that 10 extra txs got included
    assert!(block.includes_vec(txs));

    Ok(())
}
