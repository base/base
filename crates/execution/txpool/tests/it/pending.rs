use assert_matches::assert_matches;
use base_execution_txpool::{
    TransactionOrigin, TransactionPool,
    test_utils::{MockTransactionFactory, TestPoolBuilder},
};

#[tokio::test(flavor = "multi_thread")]
async fn txpool_new_pending_txs() {
    let txpool = TestPoolBuilder::default();
    let mut mock_tx_factory = MockTransactionFactory::default();
    let transaction = mock_tx_factory.create_eip1559();

    let added_result =
        txpool.add_transaction(TransactionOrigin::External, transaction.transaction.clone()).await;
    assert_matches!(added_result, Ok(outcome) if outcome.hash == *transaction.transaction.hash());

    let mut best_txns = txpool.best_transactions();
    assert_matches!(best_txns.next(), Some(tx) if tx.transaction.hash() == transaction.transaction.hash());
    assert_matches!(best_txns.next(), None);
    let transaction = mock_tx_factory.create_eip1559();
    let added_result =
        txpool.add_transaction(TransactionOrigin::External, transaction.transaction.clone()).await;
    assert_matches!(added_result, Ok(outcome) if outcome.hash == *transaction.transaction.hash());
    assert_matches!(best_txns.next(), Some(tx) if tx.transaction.hash() == transaction.transaction.hash());
}
