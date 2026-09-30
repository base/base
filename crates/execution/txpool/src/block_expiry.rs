//! Block and flashblock expiry index for validity-predicate transactions.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::TxHash;

/// Coordinates successful flashblock publication with deadline eviction and admission.
pub trait FlashblockExpiry: Clone + Send + Sync + 'static {
    /// Publishes while holding the admission lock, then evicts expired transactions.
    /// A failed publication must leave the pool unchanged.
    fn publish_and_expire<R, E>(
        &self,
        block_number: u64,
        flashblock_index: u64,
        publish: impl FnOnce() -> Result<R, E>,
    ) -> Result<(R, usize), E>;
}

/// Tracks validity transactions by their last eligible block and, when bounded,
/// flashblock. Flashblock indices reset in each block; the key is therefore
/// ordered by block first, then index. Block-only deadlines use `u64::MAX`
/// so they remain eligible until the chain advances to the next block.
#[derive(Debug, Default)]
pub struct BlockExpiryIndex {
    /// Inclusive last eligible position for each transaction.
    by_position: BTreeMap<(u64, u64), HashSet<TxHash>>,
    /// Reverse map for replacement, inclusion and explicit removal.
    by_hash: HashMap<TxHash, (u64, u64)>,
}

impl BlockExpiryIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `hash` through the inclusive `last_valid_block`.
    pub fn insert(&mut self, hash: TxHash, last_valid_block: u64) {
        self.insert_with_flashblock(hash, last_valid_block, None);
    }

    /// Registers an inclusive block and optional flashblock deadline.
    pub fn insert_with_flashblock(
        &mut self,
        hash: TxHash,
        last_valid_block: u64,
        last_valid_flashblock: Option<u64>,
    ) {
        let position = (last_valid_block, last_valid_flashblock.unwrap_or(u64::MAX));
        self.remove(&hash);
        self.by_hash.insert(hash, position);
        self.by_position.entry(position).or_default().insert(hash);
    }

    /// Removes `hash` from the index if present.
    pub fn remove(&mut self, hash: &TxHash) {
        if let Some(position) = self.by_hash.remove(hash) {
            self.remove_from_position(position, hash);
        }
    }

    /// Removes deadlines strictly before `current_block`.
    pub fn drain_expired(&mut self, current_block: u64) -> Vec<TxHash> {
        self.drain_before((current_block, 0))
    }

    /// Removes deadlines through the flashblock that was just published.
    /// Block-only deadlines are retained until the next block.
    pub fn drain_published(&mut self, block: u64, flashblock: u64) -> Vec<TxHash> {
        let cutoff = flashblock
            .checked_add(1)
            .map_or_else(|| (block.saturating_add(1), 0), |next| (block, next));
        self.drain_before(cutoff)
    }

    fn drain_before(&mut self, cutoff: (u64, u64)) -> Vec<TxHash> {
        let live = self.by_position.split_off(&cutoff);
        let expired_positions = std::mem::replace(&mut self.by_position, live);
        let mut expired = Vec::new();
        for (_, hashes) in expired_positions {
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

    fn remove_from_position(&mut self, position: (u64, u64), hash: &TxHash) {
        if let Some(hashes) = self.by_position.get_mut(&position) {
            hashes.remove(hash);
            if hashes.is_empty() {
                self.by_position.remove(&position);
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
    #[test]
    fn flashblock_deadline_respects_block_reset_and_inclusive_index() {
        let mut index = BlockExpiryIndex::new();
        index.insert_with_flashblock(hash(1), 101, Some(2));
        index.insert_with_flashblock(hash(2), 100, Some(2));
        index.insert(hash(3), 100);
        assert!(index.drain_published(100, 1).is_empty());
        assert_eq!(index.drain_published(100, 2), vec![hash(2)]);
        assert!(index.drain_published(100, 9).is_empty());
        assert!(index.drain_expired(101).contains(&hash(3)));
        assert!(index.drain_published(101, 1).is_empty());
        assert_eq!(index.drain_published(101, 2), vec![hash(1)]);
    }

    #[test]
    fn replaced_flashblock_deadline_cannot_evict_new_hash() {
        let mut index = BlockExpiryIndex::new();
        index.insert_with_flashblock(hash(1), 100, Some(1));
        index.remove(&hash(1));
        index.insert_with_flashblock(hash(2), 100, Some(3));
        assert!(index.drain_published(100, 1).is_empty());
        assert_eq!(index.drain_published(100, 3), vec![hash(2)]);
    }
}
