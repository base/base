//! Shared, cross-block cache of permanently rejected transaction hashes.

use std::time::Duration;

use alloy_primitives::TxHash;
use moka::sync::Cache;

/// Shared, cross-block cache of permanently rejected transaction hashes.
///
/// Backed by [`moka::sync::Cache`] with a TTL so entries expire if metering
/// predictions or operator limits change.
#[derive(Clone, Debug)]
pub struct RejectionCache(Cache<TxHash, ()>);

impl RejectionCache {
    /// Creates a new [`RejectionCache`] with the given capacity and TTL.
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        Self(Cache::builder().max_capacity(max_capacity).time_to_live(ttl).build())
    }

    /// Checks if a transaction hash is in the cache.
    pub fn contains_key(&self, hash: &TxHash) -> bool {
        self.0.contains_key(hash)
    }

    /// Adds a transaction hash to the cache.
    pub fn insert(&self, hash: TxHash) {
        self.0.insert(hash, ());
    }

    /// Returns the number of cached entries.
    pub fn entry_count(&self) -> u64 {
        self.0.entry_count()
    }

    /// Flushes pending cache maintenance tasks (evictions, TTL expiry).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn run_pending_tasks(&self) {
        self.0.run_pending_tasks();
    }
}
