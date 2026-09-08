//! Pools using real Base transaction representations and the shared mock validator.

use base_common_consensus::{BaseTxEnvelope, EthereumTxEnvelope, TxEip4844};
use base_execution_txpool::{
    CoinbaseTipOrdering, InMemoryBlobStore, MockTransactionValidator, Pool, PoolTransaction,
    test_utils::BaseTestTransaction,
};

/// Pool accepting Base transactions for network tests.
pub type TestPool = Pool<
    MockTransactionValidator<BaseTestTransaction>,
    CoinbaseTipOrdering<BaseTestTransaction>,
    InMemoryBlobStore,
>;

/// Constructs Base fixtures for networking tests.
#[derive(Debug)]
pub struct NetworkTestData;

impl NetworkTestData {
    /// Creates a pool using the shared mock validator.
    pub fn pool() -> TestPool {
        Pool::new(
            MockTransactionValidator::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        )
    }

    /// Converts a supported mock transaction into its Base pool representation.
    pub fn transaction<P: PoolTransaction<Consensus = EthereumTxEnvelope<TxEip4844>>>(
        transaction: P,
    ) -> BaseTestTransaction {
        let transaction = transaction.into_consensus().map(|tx| {
            BaseTxEnvelope::try_from(base_common_consensus::TxEnvelope::from(tx))
                .expect("network fixture must use a Base transaction")
        });
        BaseTestTransaction::try_from_consensus(transaction)
            .expect("network fixture must be poolable")
    }
}
