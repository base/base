//! Mempool trait and configuration for user operations.

use std::sync::Arc;

use crate::types::{UserOpHash, WrappedUserOperation};

/// Configuration for the user operation mempool.
#[derive(Debug, Default)]
pub struct PoolConfig {
    /// The minimum max fee per gas required for a user operation to be accepted.
    pub minimum_max_fee_per_gas: u128,
}

/// A mempool for storing and managing user operations.
pub trait Mempool: Send + Sync {
    /// Adds a user operation to the mempool.
    fn add_operation(&mut self, operation: &WrappedUserOperation) -> Result<(), anyhow::Error>;

    /// Returns an iterator over the top `n` user operations by priority.
    fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>>;

    /// Removes a user operation from the mempool by its hash.
    fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<WrappedUserOperation>, anyhow::Error>;
}
