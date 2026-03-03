//! Cache for flashblocks that arrive before their canonical block.

use std::collections::HashMap;

use alloy_primitives::BlockNumber;
use base_primitives::Flashblock;

/// Maximum number of blocks ahead of the latest canonical block for which
/// flashblocks may be cached. Flashblocks further ahead than this are rejected
/// to avoid unbounded memory growth during syncing.
const MAX_CACHE_AHEAD_BLOCKS: u64 = 5;

/// Buffers flashblocks that arrive before their parent canonical block has been
/// processed. Once the canonical block lands, the caller drains the
/// corresponding entries and feeds them through normal execution.
#[derive(Debug)]
pub struct FlashblockCache {
    /// Flashblocks keyed by block number, then by flashblock index. Using a
    /// nested map deduplicates by index — a later flashblock with the same
    /// index silently replaces the earlier one.
    entries: HashMap<BlockNumber, HashMap<u64, Flashblock>>,

    /// The latest canonical block number we have observed, used to decide
    /// whether a flashblock is close enough to cache.
    latest_canonical: Option<BlockNumber>,
}

impl FlashblockCache {
    /// Creates a new cache initialized with the given canonical block number.
    pub fn new(latest_canonical: BlockNumber) -> Self {
        Self { entries: HashMap::new(), latest_canonical: Some(latest_canonical) }
    }

    /// Returns `true` when the flashblock is cached.
    pub fn has_flashblock(&self, block_number: BlockNumber, index: u64) -> bool {
        self.entries.get(&block_number).and_then(|by_index| by_index.get(&index)).is_some()
    }

    /// Returns `true` when the flashblock's block number is within
    /// [`MAX_CACHE_AHEAD_BLOCKS`] of the latest known canonical block and is
    /// therefore eligible for caching.
    pub const fn is_cacheable(&self, block_number: BlockNumber) -> bool {
        match self.latest_canonical {
            Some(canonical) => block_number <= canonical + MAX_CACHE_AHEAD_BLOCKS + 1,
            None => false,
        }
    }

    /// Inserts a flashblock into the cache.
    ///
    /// Returns `true` if the flashblock was cached, `false` if it was rejected
    /// because its block number exceeds the cache-ahead limit.
    pub fn insert(&mut self, flashblock: Flashblock) -> bool {
        let block_number = flashblock.metadata.block_number;
        if !self.is_cacheable(block_number) {
            return false;
        }
        let min_block_number_to_retain = block_number.saturating_sub(MAX_CACHE_AHEAD_BLOCKS);
        self.entries.retain(|&bn, _| bn > min_block_number_to_retain);
        self.entries.entry(block_number).or_default().insert(flashblock.index, flashblock);
        true
    }

    /// Drains all cached flashblocks for the given block number, returning them
    /// sorted by index. Returns an empty `Vec` when nothing is cached.
    pub fn drain(&mut self, block_number: BlockNumber) -> Vec<Flashblock> {
        let Some(by_index) = self.entries.remove(&block_number) else {
            return Vec::new();
        };
        let mut flashblocks: Vec<Flashblock> = by_index.into_values().collect();
        flashblocks.sort_by_key(|fb| fb.index);
        flashblocks
    }

    /// Updates the latest canonical block number and evicts any cached entries
    /// at or below it (they can no longer be useful).
    pub fn update_canonical(&mut self, block_number: BlockNumber) {
        self.latest_canonical = Some(block_number);
        self.entries.retain(|&bn, _| bn > block_number);
    }

    /// Returns the number of distinct block numbers currently cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the total number of individual flashblocks cached across all
    /// block numbers.
    pub fn total_flashblocks(&self) -> usize {
        self.entries.values().map(|by_index| by_index.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_engine::PayloadId;
    use base_primitives::{ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;

    fn make_flashblock(block_number: u64, index: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata { block_number },
        }
    }

    #[test]
    fn insert_and_drain() {
        let mut cache = FlashblockCache::new(10);

        assert!(cache.insert(make_flashblock(11, 1)));
        assert!(cache.insert(make_flashblock(11, 0)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_flashblocks(), 2);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 2);
        // Should be sorted by index
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn drain_empty() {
        let mut cache = FlashblockCache::new(0);
        let drained = cache.drain(42);
        assert!(drained.is_empty());
    }

    #[test]
    fn rejects_beyond_cache_limit() {
        let mut cache = FlashblockCache::new(10);

        // Block 16 is the last cacheable block (10 + 5 + 1)
        assert!(cache.insert(make_flashblock(16, 0)));
        // Block 17 exceeds the limit
        assert!(!cache.insert(make_flashblock(17, 0)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn update_canonical_evicts_old_entries() {
        let mut cache = FlashblockCache::new(10);

        assert!(cache.insert(make_flashblock(11, 0)));
        assert!(cache.insert(make_flashblock(12, 0)));
        assert!(cache.insert(make_flashblock(13, 0)));
        assert_eq!(cache.len(), 3);

        // Advancing canonical to 12 should evict blocks 11 and 12
        cache.update_canonical(12);
        assert_eq!(cache.len(), 1);
        assert!(cache.drain(11).is_empty());
        assert!(cache.drain(12).is_empty());
        assert_eq!(cache.drain(13).len(), 1);
    }

    #[test]
    fn not_cacheable_without_canonical() {
        let mut cache = FlashblockCache { entries: HashMap::new(), latest_canonical: None };
        assert!(!cache.is_cacheable(1));
        assert!(!cache.insert(make_flashblock(1, 0)));
    }

    #[test]
    fn duplicate_index_keeps_latest() {
        let mut cache = FlashblockCache::new(10);

        let mut fb_old = make_flashblock(11, 0);
        fb_old.diff.state_root = alloy_primitives::B256::ZERO;
        let mut fb_new = make_flashblock(11, 0);
        fb_new.diff.state_root = alloy_primitives::B256::with_last_byte(1);

        assert!(cache.insert(fb_old));
        assert!(cache.insert(fb_new.clone()));
        assert_eq!(cache.total_flashblocks(), 1);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].diff.state_root, fb_new.diff.state_root);
    }

    #[test]
    fn multiple_flashblocks_same_block() {
        let mut cache = FlashblockCache::new(10);

        assert!(cache.insert(make_flashblock(11, 0)));
        assert!(cache.insert(make_flashblock(11, 1)));
        assert!(cache.insert(make_flashblock(11, 2)));
        assert_eq!(cache.total_flashblocks(), 3);
        assert_eq!(cache.len(), 1);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert_eq!(drained[2].index, 2);
    }
}
