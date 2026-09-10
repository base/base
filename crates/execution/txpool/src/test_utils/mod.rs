//! Internal helpers for testing.

use std::ops::Deref;

/// Base transaction cache used by shared execution and network fixtures.
pub use crate::BasePooledTransaction as BaseTestTransaction;
use crate::{Pool, PoolConfig, blobstore::InMemoryBlobStore, noop::MockTransactionValidator};

mod tx_gen;
pub use tx_gen::*;

mod mock;
pub use mock::*;

mod okvalidator;
pub use okvalidator::*;

/// A [Pool] used for testing
pub type TestPool = Pool<InMemoryBlobStore>;

/// Structure encapsulating a [`TestPool`] used for testing
#[derive(Debug, Clone)]
pub struct TestPoolBuilder(TestPool);

impl Default for TestPoolBuilder {
    fn default() -> Self {
        Self(Pool::new_test(
            MockTransactionValidator::default(),
            MockOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        ))
    }
}

impl TestPoolBuilder {
    /// Returns a new [`TestPoolBuilder`] with a custom validator used for testing purposes
    pub fn with_validator(self, validator: MockTransactionValidator) -> Self {
        Self(Pool::new_test(
            validator,
            MockOrdering::default(),
            self.pool.blob_store().clone(),
            self.pool.config().clone(),
        ))
    }

    /// Returns a new [`TestPoolBuilder`] with a custom ordering used for testing purposes
    pub fn with_ordering(self, ordering: MockOrdering) -> Self {
        Self(Pool::new_test(
            self.pool.validator().clone(),
            ordering,
            self.pool.blob_store().clone(),
            self.pool.config().clone(),
        ))
    }

    /// Returns a new [`TestPoolBuilder`] with a custom blob store used for testing purposes
    pub fn with_blob_store(self, blob_store: InMemoryBlobStore) -> Self {
        Self(Pool::new_test(
            self.pool.validator().clone(),
            MockOrdering::default(),
            blob_store,
            self.pool.config().clone(),
        ))
    }

    /// Returns a new [`TestPoolBuilder`] with a custom configuration used for testing purposes
    pub fn with_config(self, config: PoolConfig) -> Self {
        Self(Pool::new_test(
            self.pool.validator().clone(),
            MockOrdering::default(),
            self.pool.blob_store().clone(),
            config,
        ))
    }
}

impl From<TestPoolBuilder> for TestPool {
    fn from(wrapper: TestPoolBuilder) -> Self {
        wrapper.0
    }
}

impl Deref for TestPoolBuilder {
    type Target = TestPool;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Returns a new [Pool] with default field values used for testing purposes
pub fn testing_pool() -> TestPool {
    TestPoolBuilder::default().into()
}

mod pool_validator;
pub use pool_validator::PoolValidator;
