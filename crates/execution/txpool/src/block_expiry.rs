//! Block-number expiry index for validity-predicate transactions.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::TxHash;

/// Tracks validity transactions by the last block in which they can be included,
/// so the pool can evict them once the chain commits that block.
#[derive(Debug, Default)]
pub struct BlockExpiryIndex {
    /// Transactions keyed by their inclusive last eligible block.
    by_block: BTreeMap<u64, HashSet<TxHash>>,
    /// Reverse map for replacement, inclusion and explicit removal.
    by_hash: HashMap<TxHash, u64>,
}

impl BlockExpiryIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `hash` as valid through (and including) `last_valid_block`.
    ///
    /// Re-registering a hash replaces its previous bound.
    pub fn insert(&mut self, hash: TxHash, last_valid_block: u64) {
        self.remove(&hash);
        self.by_hash.insert(hash, last_valid_block);
        self.by_block.entry(last_valid_block).or_default().insert(hash);
    }

    /// Removes `hash` from the index if present.
    pub fn remove(&mut self, hash: &TxHash) {
        let Some(block) = self.by_hash.remove(hash) else { return };
        if let Some(hashes) = self.by_block.get_mut(&block) {
            hashes.remove(hash);
            if hashes.is_empty() {
                self.by_block.remove(&block);
            }
        }
    }

    /// Removes deadlines at or before `committed_block`, since they cannot hold in the next block.
    pub fn drain_expired(&mut self, committed_block: u64) -> Vec<TxHash> {
        let Some(next_block) = committed_block.checked_add(1) else {
            self.by_block.clear();
            return std::mem::take(&mut self.by_hash).into_keys().collect();
        };
        let live = self.by_block.split_off(&next_block);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    #[test]
    fn drains_blocks_through_committed_head() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(2), 101);
        index.insert(hash(3), 99);

        // After block 99 commits, deadlines in blocks 100 and 101 remain live.
        let expired = index.drain_expired(99);
        assert_eq!(expired, vec![hash(3)]);
        assert_eq!(index.len(), 2);

        // Committing block 101 expires both remaining deadlines.
        let mut expired = index.drain_expired(101);
        expired.sort();
        assert_eq!(expired, vec![hash(1), hash(2)]);
        assert!(index.is_empty());
    }

    #[test]
    fn tx_expires_when_its_last_valid_block_commits() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);

        assert!(index.drain_expired(99).is_empty());
        assert_eq!(index.drain_expired(100), vec![hash(1)]);
    }

    #[test]
    fn reinsert_replaces_previous_bound() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(1), 200);

        assert!(index.drain_expired(150).is_empty());
        assert_eq!(index.len(), 1);
        assert_eq!(index.drain_expired(200), vec![hash(1)]);
    }

    #[test]
    fn remove_drops_tracking() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.insert(hash(2), 100);
        index.remove(&hash(1));

        assert_eq!(index.len(), 1);
        assert_eq!(index.drain_expired(100), vec![hash(2)]);
    }

    #[test]
    fn remove_of_unknown_hash_is_noop() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), 100);
        index.remove(&hash(9));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn maximum_block_deadline_expires_without_overflow() {
        let mut index = BlockExpiryIndex::new();
        index.insert(hash(1), u64::MAX);
        index.insert(hash(2), u64::MAX - 1);

        assert_eq!(index.drain_expired(u64::MAX - 1), vec![hash(2)]);
        assert_eq!(index.drain_expired(u64::MAX), vec![hash(1)]);
        assert!(index.is_empty());
    }
}
