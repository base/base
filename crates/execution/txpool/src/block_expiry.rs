//! Block-height expiry index for validity-predicate transactions.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::TxHash;

/// Tracks validity-predicate transactions by the last block at which they can
/// still be included, so the pool can evict them once the chain advances past
/// that block.
///
/// This is the pool-side, block-granular counterpart to the builder's
/// [`ValidityPredicate::is_batch_expired`](crate::ValidityPredicate::is_batch_expired)
/// check. Unlike the EIP-8130 invalidation guard it is not gated on transaction
/// type, so it works for the EIP-1559 transactions used by the beta advanced
/// submission path.
#[derive(Debug, Default)]
pub struct BlockExpiryIndex {
    /// Maps a transaction's inclusive last-valid block number to the set of
    /// transactions that expire after it.
    by_block: BTreeMap<u64, HashSet<TxHash>>,
    /// Reverse map for removal when a transaction leaves the pool by another
    /// path (inclusion, replacement, guard eviction).
    by_hash: HashMap<TxHash, u64>,
}

impl BlockExpiryIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `hash` as valid only through (and including) `last_valid_block`.
    ///
    /// Re-registering a hash replaces its previous bound.
    pub fn insert(&mut self, hash: TxHash, last_valid_block: u64) {
        if let Some(previous) = self.by_hash.insert(hash, last_valid_block) {
            self.remove_from_block(previous, &hash);
        }
        self.by_block.entry(last_valid_block).or_default().insert(hash);
    }

    /// Removes `hash` from the index if present.
    pub fn remove(&mut self, hash: &TxHash) {
        if let Some(block) = self.by_hash.remove(hash) {
            self.remove_from_block(block, hash);
        }
    }

    /// Removes and returns every transaction that can no longer be included at
    /// `current_block` — that is, whose last-valid block is strictly before it.
    pub fn drain_expired(&mut self, current_block: u64) -> Vec<TxHash> {
        // Keys `>= current_block` are still valid at `current_block`; keep them.
        let live = self.by_block.split_off(&current_block);
        let expired_blocks = std::mem::replace(&mut self.by_block, live);
        let mut expired = Vec::new();
        for (_, hashes) in expired_blocks {
            for hash in hashes {
                self.by_hash.remove(&hash);
                expired.push(hash);
            }
        }
        expired
    }

    /// Returns the number of tracked transactions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Returns whether the index tracks no transactions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    fn remove_from_block(&mut self, block: u64, hash: &TxHash) {
        if let Some(hashes) = self.by_block.get_mut(&block) {
            hashes.remove(hash);
            if hashes.is_empty() {
                self.by_block.remove(&block);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    #[test]
    fn drains_only_blocks_before_current() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(2), 101);
        index.insert(hash(3), 99);

        // At block 100, only the tx valid through 99 is expired.
        let expired = index.drain_expired(100);
        assert_eq!(expired, vec![hash(3)]);
        assert_eq!(index.len(), 2);

        // At block 102, both remaining are expired.
        let mut expired = index.drain_expired(102);
        expired.sort();
        assert_eq!(expired, vec![hash(1), hash(2)]);
        assert!(index.is_empty());
    }

    #[test]
    fn tx_valid_through_current_block_is_not_expired() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);

        assert!(index.drain_expired(100).is_empty());
        assert_eq!(index.drain_expired(101), vec![hash(1)]);
    }

    #[test]
    fn reinsert_replaces_previous_bound() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(1), 200);

        assert!(index.drain_expired(150).is_empty());
        assert_eq!(index.len(), 1);
        assert_eq!(index.drain_expired(201), vec![hash(1)]);
    }

    #[test]
    fn remove_drops_tracking() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(2), 100);
        index.remove(&hash(1));

        assert_eq!(index.len(), 1);
        assert_eq!(index.drain_expired(101), vec![hash(2)]);
    }

    #[test]
    fn remove_of_unknown_hash_is_noop() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.remove(&hash(9));
        assert_eq!(index.len(), 1);
    }
}
