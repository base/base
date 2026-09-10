//! Unsupported blob transactions are rejected before entering the Base pool.

use alloy_eips::eip2718::Encodable2718;
use base_execution_txpool_pool::{
    BasePooledTransaction, RawPoolTransactionError,
    test_utils::{TestPoolBuilder, TransactionBuilder},
};

#[tokio::test]
async fn rejects_blob_transaction_before_pool_admission() {
    let pool = TestPoolBuilder::default();
    let blob = TransactionBuilder::default().into_eip4844();
    assert!(matches!(
        BasePooledTransaction::recover_raw_transaction(&blob.encoded_2718()),
        Err(RawPoolTransactionError::FailedToDecodeSignedTransaction)
    ));
    assert!(pool.is_empty());
}
