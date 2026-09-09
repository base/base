//! Pools using real Base transaction representations and the shared mock validator.

use base_execution_txpool::{
    CoinbaseTipOrdering, InMemoryBlobStore, MockTransactionValidator, Pool,
    test_utils::BaseTestTransaction,
};

/// Pool accepting Base transactions for network tests.
pub type TestPool = Pool<MockTransactionValidator, CoinbaseTipOrdering, InMemoryBlobStore>;

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
    pub fn transaction<T>(transaction: T) -> BaseTestTransaction
    where
        T: TryInto<BaseTestTransaction, Error: std::fmt::Debug>,
    {
        transaction.try_into().expect("network fixture must use a Base transaction")
    }
}
