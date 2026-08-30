//! Cache for flashblocks that arrive before their canonical block.

use std::collections::HashMap;

use alloy_primitives::{B256, BlockNumber};
use alloy_rpc_types_engine::PayloadId;
use base_common_flashblocks::{Flashblock, MAX_DECOMPRESSED_FLASHBLOCK_BYTES};

/// Maximum number of blocks ahead of the latest canonical block for which
/// flashblocks may be cached. Flashblocks further ahead than this are rejected
/// to avoid unbounded memory growth during syncing.
const MAX_CACHE_AHEAD_BLOCKS: u64 = 5;

/// Maximum payload attempts retained for one block number.
const MAX_PAYLOADS_PER_BLOCK: usize = 8;

/// Maximum flashblock indices retained for one payload attempt.
const MAX_FLASHBLOCKS_PER_PAYLOAD: u64 = 16;

/// Worst-case decoded bytes retained by the cache.
const MAX_CACHE_BYTES: usize = 80 * 1024 * 1024;

/// Maximum cached messages, derived from the decoder's per-message byte limit.
const MAX_TOTAL_CACHED_FLASHBLOCKS: usize = MAX_CACHE_BYTES / MAX_DECOMPRESSED_FLASHBLOCK_BYTES;

/// Flashblocks from one payload, keyed by their sequence index.
pub type CachedPayloadFlashblocks = HashMap<u64, Flashblock>;

/// Cached payload sequence number and its flashblocks.
pub type CachedPayload = (u64, CachedPayloadFlashblocks);

/// Cached payload attempts keyed by payload ID.
pub type CachedPayloads = HashMap<PayloadId, CachedPayload>;

/// Buffers flashblocks that arrive before their parent canonical block has been
/// processed. Once the canonical block lands, the caller drains the
/// corresponding entries and feeds them through normal execution.
#[derive(Debug)]
pub struct FlashblockCache {
    /// Flashblocks keyed by block number, payload ID, then flashblock index.
    ///
    /// Each payload stores the arrival sequence of its index-zero base so a
    /// replacement build is preferred over an older, longer attempt.
    entries: HashMap<BlockNumber, CachedPayloads>,

    /// Monotonic sequence assigned to newly observed payload bases.
    next_payload_sequence: u64,

    /// The latest canonical block number we have observed, used to decide
    /// whether a flashblock is close enough to cache.
    latest_canonical: Option<BlockNumber>,
}

impl FlashblockCache {
    /// Creates a new cache initialized with the given canonical block number.
    pub fn new(latest_canonical: BlockNumber) -> Self {
        Self {
            entries: HashMap::new(),
            next_payload_sequence: 0,
            latest_canonical: Some(latest_canonical),
        }
    }

    /// Returns `true` when the flashblock is cached.
    pub fn has_flashblock(
        &self,
        block_number: BlockNumber,
        payload_id: PayloadId,
        index: u64,
    ) -> bool {
        self.entries
            .get(&block_number)
            .and_then(|by_payload| by_payload.get(&payload_id))
            .and_then(|(_, by_index)| by_index.get(&index))
            .is_some()
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
        if !self.is_cacheable(block_number) || flashblock.index >= MAX_FLASHBLOCKS_PER_PAYLOAD {
            return false;
        }
        let min_block_number_to_retain = block_number.saturating_sub(MAX_CACHE_AHEAD_BLOCKS);
        self.entries.retain(|&bn, _| bn > min_block_number_to_retain);
        let by_payload = self.entries.entry(block_number).or_default();
        if !by_payload.contains_key(&flashblock.payload_id)
            && by_payload.len() >= MAX_PAYLOADS_PER_BLOCK
            && let Some(oldest) = by_payload
                .iter()
                .min_by_key(|(_, (base_sequence, _))| *base_sequence)
                .map(|(payload_id, _)| *payload_id)
        {
            by_payload.remove(&oldest);
        }
        let payload = by_payload.entry(flashblock.payload_id).or_insert_with(|| {
            self.next_payload_sequence = self.next_payload_sequence.saturating_add(1);
            (self.next_payload_sequence, HashMap::new())
        });
        if flashblock.index == 0 && !payload.1.is_empty() {
            self.next_payload_sequence = self.next_payload_sequence.saturating_add(1);
            payload.0 = self.next_payload_sequence;
        }
        payload.1.insert(flashblock.index, flashblock);
        while self.total_flashblocks() > MAX_TOTAL_CACHED_FLASHBLOCKS {
            let oldest = self
                .entries
                .iter()
                .flat_map(|(block_number, by_payload)| {
                    by_payload.iter().map(move |(payload_id, (base_sequence, _))| {
                        (*base_sequence, *block_number, *payload_id)
                    })
                })
                .min_by_key(|(base_sequence, _, _)| *base_sequence);
            let Some((_, oldest_block, oldest_payload)) = oldest else {
                break;
            };
            if let Some(by_payload) = self.entries.get_mut(&oldest_block) {
                by_payload.remove(&oldest_payload);
                if by_payload.is_empty() {
                    self.entries.remove(&oldest_block);
                }
            }
        }
        true
    }

    /// Drains the newest cached payload whose base names `parent_hash`.
    ///
    /// Flashblocks from different payload IDs are never combined, and older
    /// alternatives remain cached until one can be replayed successfully.
    pub fn drain(&mut self, block_number: BlockNumber, parent_hash: B256) -> Vec<Flashblock> {
        let Some(payload_id) = self.entries.get(&block_number).and_then(|by_payload| {
            by_payload
                .iter()
                .filter(|(_, (_, by_index))| {
                    by_index
                        .get(&0)
                        .and_then(|flashblock| flashblock.base.as_ref())
                        .is_some_and(|base| base.parent_hash == parent_hash)
                })
                .max_by_key(|(_, (base_sequence, _))| *base_sequence)
                .map(|(payload_id, _)| *payload_id)
        }) else {
            return Vec::new();
        };
        let by_index = self
            .entries
            .get_mut(&block_number)
            .and_then(|by_payload| by_payload.remove(&payload_id))
            .map(|(_, by_index)| by_index)
            .unwrap_or_default();
        if self.entries.get(&block_number).is_some_and(HashMap::is_empty) {
            self.entries.remove(&block_number);
        }
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

    /// Returns the latest canonical block number observed by this cache.
    pub const fn latest_canonical_number(&self) -> Option<BlockNumber> {
        self.latest_canonical
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
        self.entries.values().flat_map(HashMap::values).map(|(_, by_index)| by_index.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata,
    };

    use super::*;

    fn make_flashblock(block_number: u64, index: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: (index == 0).then_some(ExecutionPayloadBaseV1 {
                parent_hash: B256::ZERO,
                block_number,
                ..Default::default()
            }),
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

        let drained = cache.drain(11, B256::ZERO);
        assert_eq!(drained.len(), 2);
        // Should be sorted by index
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn drain_empty() {
        let mut cache = FlashblockCache::new(0);
        let drained = cache.drain(42, B256::ZERO);
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
        assert!(cache.drain(11, B256::ZERO).is_empty());
        assert!(cache.drain(12, B256::ZERO).is_empty());
        assert_eq!(cache.drain(13, B256::ZERO).len(), 1);
    }

    #[test]
    fn not_cacheable_without_canonical() {
        let mut cache = FlashblockCache {
            entries: HashMap::new(),
            next_payload_sequence: 0,
            latest_canonical: None,
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

        let drained = cache.drain(11, B256::ZERO);
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

        let drained = cache.drain(11, B256::ZERO);
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert_eq!(drained[2].index, 2);
    }

    #[test]
    fn payloads_are_not_mixed() {
        let mut cache = FlashblockCache::new(10);
        let mut first_base = make_flashblock(11, 0);
        first_base.payload_id = PayloadId::new([1; 8]);
        let mut first_delta = make_flashblock(11, 1);
        first_delta.payload_id = PayloadId::new([1; 8]);
        let mut second_delta = make_flashblock(11, 2);
        second_delta.payload_id = PayloadId::new([2; 8]);

        assert!(cache.insert(first_base));
        assert!(cache.insert(first_delta));
        assert!(cache.insert(second_delta));

        let drained = cache.drain(11, B256::ZERO);
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().all(|flashblock| flashblock.payload_id == PayloadId::new([1; 8])));
    }

    #[test]
    fn newest_base_wins_over_longer_abandoned_payload() {
        let mut cache = FlashblockCache::new(10);
        for index in 0..3 {
            let mut flashblock = make_flashblock(11, index);
            flashblock.payload_id = PayloadId::new([1; 8]);
            assert!(cache.insert(flashblock));
        }
        let mut replacement = make_flashblock(11, 0);
        replacement.payload_id = PayloadId::new([2; 8]);
        assert!(cache.insert(replacement));

        let replacement = cache.drain(11, B256::ZERO);
        assert_eq!(replacement.len(), 1);
        assert_eq!(replacement[0].payload_id, PayloadId::new([2; 8]));

        let abandoned = cache.drain(11, B256::ZERO);
        assert_eq!(abandoned.len(), 3);
        assert!(abandoned.iter().all(|flashblock| flashblock.payload_id == PayloadId::new([1; 8])));
    }

    #[test]
    fn cache_bounds_payload_attempts_and_indices() {
        let mut cache = FlashblockCache::new(10);
        for payload in 1u64..=9 {
            let mut flashblock = make_flashblock(11, 0);
            flashblock.payload_id = PayloadId::new(payload.to_be_bytes());
            assert!(cache.insert(flashblock));
        }

        assert_eq!(cache.total_flashblocks(), MAX_PAYLOADS_PER_BLOCK);
        let mut excessive_index = make_flashblock(11, MAX_FLASHBLOCKS_PER_PAYLOAD);
        excessive_index.payload_id = PayloadId::new(9u64.to_be_bytes());
        assert!(!cache.insert(excessive_index));

        for expected in (2u64..=9).rev() {
            let drained = cache.drain(11, B256::ZERO);
            assert_eq!(drained[0].payload_id, PayloadId::new(expected.to_be_bytes()));
        }
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_enforces_global_decoded_byte_budget() {
        let mut cache = FlashblockCache::new(10);
        for block_number in 11u64..=13 {
            for payload in 1u64..=8 {
                let mut flashblock = make_flashblock(block_number, 0);
                flashblock.payload_id =
                    PayloadId::new((block_number * 100 + payload).to_be_bytes());
                assert!(cache.insert(flashblock));
            }
        }

        assert!(cache.total_flashblocks() <= MAX_TOTAL_CACHED_FLASHBLOCKS);
        assert_eq!(
            MAX_TOTAL_CACHED_FLASHBLOCKS * MAX_DECOMPRESSED_FLASHBLOCK_BYTES,
            MAX_CACHE_BYTES
        );
    }
}
