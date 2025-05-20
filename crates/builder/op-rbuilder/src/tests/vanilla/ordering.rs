use crate::tests::framework::{TestHarnessBuilder, ONE_ETH};
use alloy_consensus::Transaction;
use futures::{future::join_all, stream, StreamExt};

/// This test ensures that the transactions are ordered by fee priority in the block.
#[tokio::test]
async fn fee_priority_ordering() -> eyre::Result<()> {
    let harness = TestHarnessBuilder::new("integration_test_fee_priority_ordering")
        .build()
        .await?;

    let mut generator = harness.block_generator().await?;
    let accounts = generator.create_funded_accounts(10, ONE_ETH).await?;
    let base_fee = harness.latest_base_fee().await;

    // generate transactions with randomized tips
    let txs = join_all(accounts.iter().map(|signer| {
        harness
            .create_transaction()
            .with_signer(*signer)
            .with_max_priority_fee_per_gas(rand::random_range(1..50))
            .send()
    }))
    .await
    .into_iter()
    .collect::<eyre::Result<Vec<_>>>()?
    .into_iter()
    .map(|tx| *tx.tx_hash())
    .collect::<Vec<_>>();

    generator.generate_block().await?;

    // verify all transactions are included in the block
    assert!(
        stream::iter(txs.iter())
            .all(|tx_hash| async {
                harness
                    .latest_block()
                    .await
                    .transactions
                    .hashes()
                    .any(|hash| hash == *tx_hash)
            })
            .await,
        "not all transactions included in the block"
    );

    // verify all transactions are ordered by fee priority
    let txs_tips = harness
        .latest_block()
        .await
        .into_transactions_vec()
        .into_iter()
        .skip(1) // skip the deposit transaction
        .take(txs.len()) // skip the last builder transaction
        .map(|tx| tx.effective_tip_per_gas(base_fee as u64))
        .rev() // we want to check descending order
        .collect::<Vec<_>>();

    assert!(
        txs_tips.is_sorted(),
        "Transactions not ordered by fee priority"
    );

    Ok(())
}
