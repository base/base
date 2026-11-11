//! Shared, cross-job cache of permanently rejected transaction encodings.

use std::time::Duration;

use alloy_primitives::{B256, map::FbBuildHasher};
use moka::sync::Cache;

/// Default maximum number of cached permanently rejected transaction encodings.
pub const REJECTION_CACHE_MAX_CAPACITY: u64 = 100_000;

/// Time-to-live for cached permanently rejected transaction encodings.
///
/// Entries expire so metering predictions and operator limits can change
/// without locking an encoding out indefinitely.
pub const REJECTION_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// Shared, cross-job cache of permanently rejected transaction encodings.
///
/// Keys are the hash of the exact EIP-2718 encoding ([`Encodable2718::trie_hash`]), which is the
/// transaction hash for every type except EIP-8130. EIP-8130 encodings that differ only in
/// authorization share a transaction hash but can execute and meter differently, so rejecting one
/// must not skip the others.
///
/// Backed by [`moka::sync::Cache`] with a TTL so entries expire if metering
/// predictions or operator limits change. Native payload jobs skip cached
/// encodings even if the transaction is re-gossiped into the pool. Nonce-lane
/// descendants are skipped for the **current** scan via
/// `PayloadTransactions::mark_invalid`.
///
/// Builders look up every candidate, so keys are hashed with the seeded fixed-bytes hasher the
/// other per-candidate hash sets use rather than the default `SipHash`.
///
/// [`Encodable2718::trie_hash`]: alloy_eips::eip2718::Encodable2718::trie_hash
#[derive(Clone)]
pub struct RejectionCache(Cache<B256, (), FbBuildHasher<32>>);

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
        Self(
            Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(ttl)
                .build_with_hasher(FbBuildHasher::default()),
        )
    }

    /// Returns `true` if the encoding with hash `encoded_hash` is cached as permanently rejected.
    pub fn is_rejected(&self, encoded_hash: &B256) -> bool {
        self.contains_key(encoded_hash)
    }

    /// Checks if an encoding hash is in the cache.
    pub fn contains_key(&self, encoded_hash: &B256) -> bool {
        self.0.contains_key(encoded_hash)
    }

    /// Records the encodings with hashes `encoded_hashes` as permanently rejected for later
    /// payload jobs.
    pub fn mark_rejected(&self, encoded_hashes: &[B256]) {
        for encoded_hash in encoded_hashes {
            self.insert(*encoded_hash);
        }
    }

    /// Adds an encoding hash to the cache.
    pub fn insert(&self, encoded_hash: B256) {
        self.0.insert(encoded_hash, ());
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
    use alloy_primitives::B256;

    use super::{REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL, RejectionCache};

    #[test]
    fn default_matches_named_capacity_and_ttl() {
        assert_eq!(REJECTION_CACHE_MAX_CAPACITY, 100_000);
        assert_eq!(REJECTION_CACHE_TTL, std::time::Duration::from_secs(1800));
    }

    #[test]
    fn mark_rejected_is_visible_to_later_lookups() {
        let cache = RejectionCache::default();
        let hash = B256::repeat_byte(0x11);
        let other = B256::repeat_byte(0x22);

        cache.mark_rejected(&[hash]);

        assert!(cache.is_rejected(&hash));
        assert!(!cache.is_rejected(&other));
        cache.run_pending_tasks();
        assert_eq!(cache.entry_count(), 1);
    }
}
