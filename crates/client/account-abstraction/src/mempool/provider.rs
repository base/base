//! UserOperation Mempool Provider Trait
//!
//! This module defines the `UserOpMempoolProvider` trait that abstracts access to the
//! UserOperation mempool for use by external components like the block builder.
//!
//! # Usage
//!
//! The builder can use this trait to:
//! 1. Query the best UserOps for bundling
//! 2. Mark UserOps as pending inclusion (to prevent duplicates)
//! 3. Confirm UserOps were included (removes them from pool)
//! 4. Release UserOps if the bundle failed
//!
//! # Example
//!
//! ```ignore
//! use base_account_abstraction::mempool::UserOpMempoolProvider;
//!
//! async fn build_bundle(mempool: &impl UserOpMempoolProvider) {
//!     let entry_point = ENTRYPOINT_V07_ADDRESS;
//!     
//!     // Get best UserOps
//!     let ops = mempool.get_best_userops(entry_point, 10, 30_000_000);
//!     let hashes: Vec<_> = ops.iter().map(|op| op.hash).collect();
//!     
//!     // Mark as pending before building
//!     mempool.mark_pending_inclusion(entry_point, &hashes);
//!     
//!     // Build and submit bundle...
//!     
//!     // If bundle included:
//!     mempool.confirm_included(entry_point, &hashes);
//!     
//!     // Or if bundle failed:
//!     // mempool.release_pending(entry_point, &hashes);
//! }
//! ```

use alloy_primitives::{Address, B256};

use super::pool::{PooledUserOp, UserOpPool};

/// Trait for accessing the UserOperation mempool
///
/// This trait provides the interface that the block builder uses to interact
/// with the AA mempool. It abstracts away the locking and allows for different
/// implementations (e.g., local pool, remote pool via RPC).
pub trait UserOpMempoolProvider: Send + Sync {
    /// Get the best UserOperations for bundling
    ///
    /// Returns UserOps ordered by effective gas price (highest first), filtered to:
    /// - Only "Ready" status (not pending inclusion)
    /// - Not expired
    /// - Respecting max_count and max_gas limits
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint contract address
    /// * `max_count` - Maximum number of UserOps to return
    /// * `max_gas` - Maximum total gas for returned UserOps
    fn get_best_userops(
        &self,
        entry_point: Address,
        max_count: usize,
        max_gas: u64,
    ) -> Vec<PooledUserOp>;

    /// Mark UserOps as pending inclusion
    ///
    /// Called by the builder before building a bundle to prevent the same
    /// UserOps from being included in another bundle.
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint contract address
    /// * `hashes` - UserOp hashes to mark as pending
    fn mark_pending_inclusion(&self, entry_point: Address, hashes: &[B256]);

    /// Confirm UserOps were included on-chain
    ///
    /// Called after a bundle is successfully included. Removes the UserOps
    /// from the mempool and updates reputation.
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint contract address
    /// * `hashes` - UserOp hashes that were included
    ///
    /// # Returns
    /// The removed UserOps (for logging/metrics)
    fn confirm_included(&self, entry_point: Address, hashes: &[B256]) -> Vec<PooledUserOp>;

    /// Release pending UserOps
    ///
    /// Called if a bundle build or submission failed. Returns the UserOps
    /// to "Ready" status so they can be included in another bundle.
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint contract address
    /// * `hashes` - UserOp hashes to release
    fn release_pending(&self, entry_point: Address, hashes: &[B256]);

    /// Get the total number of UserOps across all entrypoints
    fn total_count(&self) -> usize;

    /// Get the number of UserOps for a specific entrypoint
    fn count(&self, entry_point: Address) -> usize;

    /// Get all entrypoints that have UserOps in the pool
    fn entrypoints(&self) -> Vec<Address>;
}

/// A shared reference to the UserOpPool that implements the provider trait
///
/// This wrapper provides thread-safe access to the pool via `Arc<RwLock<UserOpPool>>`.
#[derive(Clone)]
pub struct SharedUserOpMempoolProvider {
    pool: std::sync::Arc<parking_lot::RwLock<UserOpPool>>,
}

impl SharedUserOpMempoolProvider {
    /// Create a new shared mempool provider
    pub fn new(pool: std::sync::Arc<parking_lot::RwLock<UserOpPool>>) -> Self {
        Self { pool }
    }

    /// Get a reference to the underlying pool
    pub fn inner(&self) -> &std::sync::Arc<parking_lot::RwLock<UserOpPool>> {
        &self.pool
    }
}

impl UserOpMempoolProvider for SharedUserOpMempoolProvider {
    fn get_best_userops(
        &self,
        entry_point: Address,
        max_count: usize,
        max_gas: u64,
    ) -> Vec<PooledUserOp> {
        let pool = self.pool.read();
        // Clone the results since we can't return references from a lock guard
        pool.get_best_userops(&entry_point, max_count, max_gas)
            .into_iter()
            .cloned()
            .collect()
    }

    fn mark_pending_inclusion(&self, entry_point: Address, hashes: &[B256]) {
        let mut pool = self.pool.write();
        pool.mark_pending_inclusion(&entry_point, hashes);
    }

    fn confirm_included(&self, entry_point: Address, hashes: &[B256]) -> Vec<PooledUserOp> {
        let mut pool = self.pool.write();
        pool.confirm_included(&entry_point, hashes)
    }

    fn release_pending(&self, entry_point: Address, hashes: &[B256]) {
        let mut pool = self.pool.write();
        pool.release_pending(&entry_point, hashes);
    }

    fn total_count(&self) -> usize {
        let pool = self.pool.read();
        pool.total_count()
    }

    fn count(&self, entry_point: Address) -> usize {
        let pool = self.pool.read();
        pool.count(&entry_point)
    }

    fn entrypoints(&self) -> Vec<Address> {
        let pool = self.pool.read();
        pool.entrypoints()
    }
}

/// Implementation for Arc<RwLock<UserOpPool>> directly
///
/// This allows using the pool directly without wrapping in SharedUserOpMempoolProvider.
impl UserOpMempoolProvider for std::sync::Arc<parking_lot::RwLock<UserOpPool>> {
    fn get_best_userops(
        &self,
        entry_point: Address,
        max_count: usize,
        max_gas: u64,
    ) -> Vec<PooledUserOp> {
        let pool = self.read();
        pool.get_best_userops(&entry_point, max_count, max_gas)
            .into_iter()
            .cloned()
            .collect()
    }

    fn mark_pending_inclusion(&self, entry_point: Address, hashes: &[B256]) {
        let mut pool = self.write();
        pool.mark_pending_inclusion(&entry_point, hashes);
    }

    fn confirm_included(&self, entry_point: Address, hashes: &[B256]) -> Vec<PooledUserOp> {
        let mut pool = self.write();
        pool.confirm_included(&entry_point, hashes)
    }

    fn release_pending(&self, entry_point: Address, hashes: &[B256]) {
        let mut pool = self.write();
        pool.release_pending(&entry_point, hashes);
    }

    fn total_count(&self) -> usize {
        let pool = self.read();
        pool.total_count()
    }

    fn count(&self, entry_point: Address) -> usize {
        let pool = self.read();
        pool.count(&entry_point)
    }

    fn entrypoints(&self) -> Vec<Address> {
        let pool = self.read();
        pool.entrypoints()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mempool::MempoolConfig;

    #[test]
    fn test_shared_provider_creation() {
        let config = MempoolConfig::default();
        let pool = UserOpPool::new_shared(config, 1);
        let provider = SharedUserOpMempoolProvider::new(pool);

        assert_eq!(provider.total_count(), 0);
        assert!(provider.entrypoints().is_empty());
    }

    #[test]
    fn test_provider_trait_is_object_safe() {
        // Ensure the trait can be used as a trait object
        fn accept_provider(_: &dyn UserOpMempoolProvider) {}

        let config = MempoolConfig::default();
        let pool = UserOpPool::new_shared(config, 1);
        let provider = SharedUserOpMempoolProvider::new(pool);

        accept_provider(&provider);
    }

    #[test]
    fn test_arc_rwlock_implements_provider() {
        // Verify Arc<RwLock<UserOpPool>> can be used directly
        let config = MempoolConfig::default();
        let pool = UserOpPool::new_shared(config, 1);

        // Use as trait object
        let provider: &dyn UserOpMempoolProvider = &pool;
        assert_eq!(provider.total_count(), 0);
    }
}
