//! Cache for flashblocks that arrive before their canonical block.

use std::collections::HashMap;

use alloy_primitives::BlockNumber;
use base_common_flashblocks::Flashblock;
#[cfg(feature = "edge-measurement")]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheInsertObservation {
    pub(crate) cached: bool,
    pub(crate) replaced_generation: Option<u64>,
    pub(crate) evicted_generations: Vec<u64>,
}

#[cfg(feature = "edge-measurement")]
type CachedFlashblock = (Flashblock, Option<u64>);

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

    #[cfg(feature = "edge-measurement")]
    generations: HashMap<BlockNumber, HashMap<u64, u64>>,

    /// The latest canonical block number we have observed, used to decide
    /// whether a flashblock is close enough to cache.
    latest_canonical: Option<BlockNumber>,
}

impl FlashblockCache {
    /// Creates a new cache initialized with the given canonical block number.
    pub fn new(latest_canonical: BlockNumber) -> Self {
        Self {
            entries: HashMap::new(),
            latest_canonical: Some(latest_canonical),
            #[cfg(feature = "edge-measurement")]
            generations: HashMap::new(),
        }
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
        #[cfg(feature = "edge-measurement")]
        {
            self.insert_observed(flashblock, None).cached
        }
        #[cfg(not(feature = "edge-measurement"))]
        {
            let block_number = flashblock.metadata.block_number;
            if !self.is_cacheable(block_number) {
                return false;
            }
            let min_block_number_to_retain = block_number.saturating_sub(MAX_CACHE_AHEAD_BLOCKS);
            self.entries.retain(|&bn, _| bn > min_block_number_to_retain);
            self.entries.entry(block_number).or_default().insert(flashblock.index, flashblock);
            true
        }
    }

    #[cfg(feature = "edge-measurement")]
    pub(crate) fn insert_observed(
        &mut self,
        flashblock: Flashblock,
        source_generation: Option<u64>,
    ) -> CacheInsertObservation {
        let block_number = flashblock.metadata.block_number;
        if !self.is_cacheable(block_number) {
            return CacheInsertObservation {
                cached: false,
                replaced_generation: None,
                evicted_generations: Vec::new(),
            };
        }

        let min_block_number_to_retain = block_number.saturating_sub(MAX_CACHE_AHEAD_BLOCKS);
        let evicted_blocks: Vec<_> =
            self.entries.keys().copied().filter(|bn| *bn <= min_block_number_to_retain).collect();
        let mut evicted_generations = Vec::new();
        for evicted_block in evicted_blocks {
            self.entries.remove(&evicted_block);
            if let Some(generations) = self.generations.remove(&evicted_block) {
                evicted_generations.extend(generations.into_values());
            }
        }

        let index = flashblock.index;
        self.entries.entry(block_number).or_default().insert(index, flashblock);
        let replaced_generation = self.generations.entry(block_number).or_default().remove(&index);
        if let Some(generation) = source_generation {
            self.generations.entry(block_number).or_default().insert(index, generation);
        }

        CacheInsertObservation { cached: true, replaced_generation, evicted_generations }
    }

    /// Drains all cached flashblocks for the given block number, returning them
    /// sorted by index. Returns an empty `Vec` when nothing is cached.
    pub fn drain(&mut self, block_number: BlockNumber) -> Vec<Flashblock> {
        #[cfg(feature = "edge-measurement")]
        {
            self.drain_observed(block_number)
                .into_iter()
                .map(|(flashblock, _)| flashblock)
                .collect()
        }
        #[cfg(not(feature = "edge-measurement"))]
        {
            let Some(by_index) = self.entries.remove(&block_number) else {
                return Vec::new();
            };
            let mut flashblocks: Vec<Flashblock> = by_index.into_values().collect();
            flashblocks.sort_by_key(|fb| fb.index);
            flashblocks
        }
    }

    #[cfg(feature = "edge-measurement")]
    pub(crate) fn drain_observed(&mut self, block_number: BlockNumber) -> Vec<CachedFlashblock> {
        let Some(by_index) = self.entries.remove(&block_number) else {
            return Vec::new();
        };
        let mut generations = self.generations.remove(&block_number).unwrap_or_default();
        let mut flashblocks: Vec<CachedFlashblock> = by_index
            .into_iter()
            .map(|(index, flashblock)| (flashblock, generations.remove(&index)))
            .collect();
        flashblocks.sort_by_key(|(flashblock, _)| flashblock.index);
        flashblocks
    }

    /// Updates the latest canonical block number and evicts any cached entries
    /// at or below it (they can no longer be useful).
    pub fn update_canonical(&mut self, block_number: BlockNumber) {
        #[cfg(feature = "edge-measurement")]
        {
            _ = self.update_canonical_observed(block_number);
        }
        #[cfg(not(feature = "edge-measurement"))]
        {
            self.latest_canonical = Some(block_number);
            self.entries.retain(|&bn, _| bn > block_number);
        }
    }

    #[cfg(feature = "edge-measurement")]
    pub(crate) fn update_canonical_observed(&mut self, block_number: BlockNumber) -> Vec<u64> {
        self.latest_canonical = Some(block_number);
        self.entries.retain(|&bn, _| bn > block_number);
        let evicted_blocks: Vec<_> =
            self.generations.keys().copied().filter(|bn| *bn <= block_number).collect();
        let mut evicted_generations = Vec::new();
        for evicted_block in evicted_blocks {
            if let Some(generations) = self.generations.remove(&evicted_block) {
                evicted_generations.extend(generations.into_values());
            }
        }
        evicted_generations
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
    use base_common_flashblocks::{ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;

    fn make_flashblock(block_number: u64, index: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata::new(block_number),
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
        let mut cache = FlashblockCache {
            entries: HashMap::new(),
            latest_canonical: None,
            #[cfg(feature = "edge-measurement")]
            generations: HashMap::new(),
        };
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

#[cfg(all(test, feature = "edge-measurement"))]
mod measurement_tests {
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;

    fn flashblock(block_number: u64, index: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata::new(block_number),
        }
    }

    #[test]
    fn observations_preserve_replacement_eviction_rejection_and_drain_order() {
        let mut cache = FlashblockCache::new(10);
        assert!(cache.insert_observed(flashblock(11, 0), Some(1)).cached);
        let replacement = cache.insert_observed(flashblock(11, 0), Some(2));
        assert_eq!(replacement.replaced_generation, Some(1));

        assert!(cache.insert_observed(flashblock(12, 0), Some(3)).cached);
        let eviction = cache.insert_observed(flashblock(16, 0), Some(4));
        assert!(eviction.cached);
        assert_eq!(eviction.evicted_generations, vec![2]);

        let rejection = cache.insert_observed(flashblock(17, 0), Some(5));
        assert!(!rejection.cached);
        assert!(rejection.evicted_generations.is_empty());

        let evicted = cache.update_canonical_observed(12);
        assert_eq!(evicted, vec![3]);
        let drained = cache.drain_observed(16);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0.index, 0);
        assert_eq!(drained[0].1, Some(4));
    }

    #[test]
    fn postcutoff_untracked_overwrite_returns_old_authority_generation() {
        let mut cache = FlashblockCache::new(10);
        assert!(cache.insert_observed(flashblock(11, 0), Some(41)).cached);

        let overwrite = cache.insert_observed(flashblock(11, 0), None);

        assert!(overwrite.cached);
        assert_eq!(overwrite.replaced_generation, Some(41));
        assert_eq!(cache.drain_observed(11)[0].1, None);
    }

    #[test]
    fn measurement_hook_does_not_change_cache_decisions_or_order() {
        let mut ordinary = FlashblockCache::new(10);
        let mut observed = FlashblockCache::new(10);
        let inputs = [flashblock(11, 2), flashblock(11, 0), flashblock(11, 1)];

        let ordinary_decisions: Vec<_> =
            inputs.iter().cloned().map(|flashblock| ordinary.insert(flashblock)).collect();
        let observed_decisions: Vec<_> = inputs
            .into_iter()
            .enumerate()
            .map(|(generation, flashblock)| {
                observed
                    .insert_observed(
                        flashblock,
                        Some(u64::try_from(generation).expect("test generation fits u64")),
                    )
                    .cached
            })
            .collect();

        let ordinary_order: Vec<_> =
            ordinary.drain(11).into_iter().map(|flashblock| flashblock.index).collect();
        let observed_order: Vec<_> = observed
            .drain_observed(11)
            .into_iter()
            .map(|(flashblock, _)| flashblock.index)
            .collect();
        let ordinary_decision_fingerprint = (ordinary_decisions, ordinary_order);
        let measurement_decision_fingerprint = (observed_decisions, observed_order);
        assert_eq!(ordinary_decision_fingerprint, measurement_decision_fingerprint);
    }
}
