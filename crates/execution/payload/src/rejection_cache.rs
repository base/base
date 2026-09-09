//! Shared, cross-job cache of permanently rejected transaction hashes.

use std::time::Duration;

use alloy_primitives::TxHash;
use moka::sync::Cache;

/// Default maximum number of cached permanently rejected transaction hashes.
pub const REJECTION_CACHE_MAX_CAPACITY: u64 = 100_000;

/// Time-to-live for cached permanently rejected transaction hashes.
///
/// Entries expire so metering predictions and operator limits can change
/// without locking a hash out indefinitely.
pub const REJECTION_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// Shared, cross-job cache of permanently rejected transaction hashes.
///
/// Backed by [`moka::sync::Cache`] with a TTL so entries expire if metering
/// predictions or operator limits change. Native payload jobs skip cached
/// hashes even if the transaction is re-gossiped into the pool. Nonce-lane
/// descendants are skipped for the **current** scan via
/// `PayloadTransactions::mark_invalid`; skipping those descendants across
/// later jobs is Flashblocks-only (its iterator consults this cache).
#[derive(Clone)]
pub struct RejectionCache(Cache<TxHash, ()>);

impl std::fmt::Debug for RejectionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectionCache").field("entry_count", &self.entry_count()).finish()
    }
}

impl Default for RejectionCache {
    fn default() -> Self {
        Self::new(REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL)
    }
}

impl RejectionCache {
    /// Creates a new [`RejectionCache`] with the given capacity and TTL.
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        Self(Cache::builder().max_capacity(max_capacity).time_to_live(ttl).build())
    }

    /// Returns `true` if `hash` is cached as permanently rejected.
    pub fn is_rejected(&self, hash: &TxHash) -> bool {
        self.contains_key(hash)
    }

    /// Checks if a transaction hash is in the cache.
    pub fn contains_key(&self, hash: &TxHash) -> bool {
        self.0.contains_key(hash)
    }

    /// Records `hashes` as permanently rejected for later payload jobs.
    pub fn mark_rejected(&self, hashes: &[TxHash]) {
        for hash in hashes {
            self.insert(*hash);
        }
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
    pub fn run_pending_tasks(&self) {
        self.0.run_pending_tasks();
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;

    use super::{REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL, RejectionCache};

    #[test]
    fn default_matches_named_capacity_and_ttl() {
        assert_eq!(REJECTION_CACHE_MAX_CAPACITY, 100_000);
        assert_eq!(REJECTION_CACHE_TTL, std::time::Duration::from_secs(1800));
    }

    #[test]
    fn mark_rejected_is_visible_to_later_lookups() {
        let cache = RejectionCache::default();
        let hash = TxHash::repeat_byte(0x11);
        let other = TxHash::repeat_byte(0x22);

        cache.mark_rejected(&[hash]);

        assert!(cache.is_rejected(&hash));
        assert!(!cache.is_rejected(&other));
        cache.run_pending_tasks();
        assert_eq!(cache.entry_count(), 1);
    }
}
