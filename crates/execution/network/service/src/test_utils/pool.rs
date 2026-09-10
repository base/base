//! Pools using real Base transaction representations and the shared mock validator.

use alloy_eip2124::Head;
use base_common_chain_config::BaseChainSpec;
use base_execution_network_wire::UnifiedStatus;
use base_execution_txpool::{
    BaseOrdering, InMemoryBlobStore, MockTransactionValidator, Pool,
    test_utils::BaseTestTransaction,
};

use crate::NetworkConfigBuilder;

/// Pool accepting Base transactions for network tests.
pub type TestPool = Pool<MockTransactionValidator, InMemoryBlobStore>;

/// Constructs Base fixtures for networking tests.
#[derive(Debug)]
pub struct NetworkTestData;

impl NetworkTestData {
    /// Creates a status announcing the Base mainnet genesis.
    pub fn status() -> UnifiedStatus {
        let spec = BaseChainSpec::mainnet();
        NetworkConfigBuilder::status(
            &spec,
            &Head {
                hash: spec.genesis_hash(),
                timestamp: spec.genesis.timestamp,
                ..Default::default()
            },
        )
    }

    /// Creates a pool using the shared mock validator.
    pub fn pool() -> TestPool {
        Pool::new(
            MockTransactionValidator::default(),
            BaseOrdering::default(),
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
