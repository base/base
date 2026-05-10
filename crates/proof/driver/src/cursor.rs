//! Contains the cursor for the derivation pipeline.
//!
//! This module provides the [`PipelineCursor`] which tracks the state of the derivation
//! pipeline including L1 origins, L2 safe heads, and caching for efficient reorg handling.

use alloc::collections::{btree_map::BTreeMap, vec_deque::VecDeque};

use alloy_consensus::{Header, Sealed};
use alloy_primitives::{B256, map::HashMap};
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::TipCursor;

/// A cursor that tracks the derivation pipeline state and progress.
///
/// The [`PipelineCursor`] maintains a cache of recent L1 origins and their corresponding
/// L2 tips to efficiently handle reorgs and provide quick access to recent derivation
/// state. It implements a capacity-bounded LRU cache to prevent unbounded memory growth.
#[derive(Debug, Clone)]
pub struct PipelineCursor {
    /// The maximum number of cached L1/L2 mappings before evicting old entries.
    pub capacity: usize,
    /// The channel timeout in blocks used for reorg recovery calculations.
    pub channel_timeout: u64,
    /// The current L1 origin block that the pipeline is processing.
    pub origin: BlockInfo,
    /// Ordered list of L1 origin block numbers for cache eviction policy.
    pub origins: VecDeque<u64>,
    /// Mapping from L1 block numbers to their corresponding [`BlockInfo`].
    pub origin_infos: HashMap<u64, BlockInfo>,
    /// Mapping from L1 origin block numbers to their corresponding L2 tips.
    pub tips: BTreeMap<u64, TipCursor>,
}

impl PipelineCursor {
    /// Creates a new pipeline cursor with the specified channel timeout and initial origin.
    pub fn new(channel_timeout: u64, origin: BlockInfo) -> Self {
        // NOTE: capacity must be greater than the `channel_timeout` to allow
        // for derivation to proceed through a deep reorg.
        // Ref: <https://specs.base.org/protocol/consensus/derivation#timeouts>
        let capacity = channel_timeout as usize + 5;

        let mut origins = VecDeque::with_capacity(capacity);
        origins.push_back(origin.number);
        let mut origin_infos = HashMap::default();
        origin_infos.insert(origin.number, origin);
        Self { capacity, channel_timeout, origin, origins, origin_infos, tips: Default::default() }
    }

    /// Returns the current L1 origin block being processed by the pipeline.
    pub const fn origin(&self) -> BlockInfo {
        self.origin
    }

    /// Returns the current L2 safe head block information.
    pub fn l2_safe_head(&self) -> &L2BlockInfo {
        &self.tip().l2_safe_head
    }

    /// Returns the sealed header of the current L2 safe head.
    pub fn l2_safe_head_header(&self) -> &Sealed<Header> {
        &self.tip().l2_safe_head_header
    }

    /// Returns the output root of the current L2 safe head.
    pub fn l2_safe_head_output_root(&self) -> &B256 {
        &self.tip().l2_safe_head_output_root
    }

    /// Returns the current L2 tip cursor containing safe head information.
    ///
    /// # Panics
    /// This method panics if called before the cursor is properly initialized with at
    /// least one L1/L2 mapping.
    pub fn tip(&self) -> &TipCursor {
        if let Some((_, l2_tip)) = self.tips.last_key_value() {
            l2_tip
        } else {
            unreachable!("cursor must be initialized with one block before advancing")
        }
    }

    /// Advances the cursor to a new L1 origin and corresponding L2 tip.
    pub fn advance(&mut self, origin: BlockInfo, l2_tip_block: TipCursor) {
        if self.tips.len() >= self.capacity {
            let key = self.origins.pop_front().unwrap();
            self.tips.remove(&key);
        }

        self.origin = origin;
        self.origins.push_back(origin.number);
        self.origin_infos.insert(origin.number, origin);
        self.tips.insert(origin.number, l2_tip_block);
    }

    /// Resets the cursor state due to an L1 reorganization.
    ///
    /// # Panics
    /// This method panics if no suitable reset target is found in the cache.
    pub fn reset(&mut self, fork_block: u64) -> (TipCursor, BlockInfo) {
        let channel_start = fork_block - self.channel_timeout;

        match self.tips.get(&channel_start) {
            Some(l2_safe_tip) => {
                // The channel start block is in the cache, we can use it to reset the cursor.
                (l2_safe_tip.clone(), self.origin_infos[&channel_start])
            }
            None => {
                // If the channel start block is not in the cache, we reset the cursor
                // to the closest known L1 block for which we have a corresponding L2 block.
                let (last_l1_known_tip, l2_known_tip) = self
                    .tips
                    .range(..=channel_start)
                    .next_back()
                    .expect("walked back to genesis without finding anchor origin block");

                (l2_known_tip.clone(), self.origin_infos[last_l1_known_tip])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, Sealable};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::*;

    /// Constructs a [`BlockInfo`] with the given block number for test purposes.
    fn block_info(number: u64) -> BlockInfo {
        BlockInfo {
            hash: B256::repeat_byte(number as u8),
            number,
            parent_hash: B256::repeat_byte(number.saturating_sub(1) as u8),
            timestamp: number * 2,
        }
    }

    /// Constructs a dummy [`TipCursor`] anchored to the given L1 block number.
    fn tip_cursor(l2_number: u64) -> TipCursor {
        let l2_block = BlockInfo {
            hash: B256::repeat_byte(l2_number as u8),
            number: l2_number,
            parent_hash: B256::ZERO,
            timestamp: l2_number * 2,
        };
        let l2_info = L2BlockInfo {
            block_info: l2_block,
            l1_origin: BlockNumHash { number: l2_number, hash: B256::ZERO },
            seq_num: 0,
        };
        let header = Header { number: l2_number, ..Default::default() }.seal_slow();
        TipCursor::new(l2_info, header, B256::repeat_byte(l2_number as u8))
    }

    #[test]
    fn new_sets_capacity_from_channel_timeout() {
        let channel_timeout = 10_u64;
        let origin = block_info(1);
        let cursor = PipelineCursor::new(channel_timeout, origin);

        // capacity must exceed channel_timeout so reorg recovery can step back far enough
        assert_eq!(cursor.capacity, channel_timeout as usize + 5);
        assert_eq!(cursor.channel_timeout, channel_timeout);
    }

    #[test]
    fn new_initialises_origin_correctly() {
        let origin = block_info(42);
        let cursor = PipelineCursor::new(5, origin);

        assert_eq!(cursor.origin(), origin);
        assert!(cursor.origins.contains(&42));
        assert_eq!(cursor.origin_infos[&42], origin);
    }

    #[test]
    fn advance_updates_origin_and_tip() {
        let origin = block_info(1);
        let mut cursor = PipelineCursor::new(5, origin);

        let tip = tip_cursor(10);
        let new_origin = block_info(2);
        cursor.advance(new_origin, tip.clone());

        assert_eq!(cursor.origin(), new_origin);
        assert_eq!(cursor.tip().l2_safe_head.block_info.number, 10);
        assert_eq!(cursor.l2_safe_head().block_info.number, 10);
    }

    #[test]
    fn advance_evicts_oldest_entry_when_at_capacity() {
        let channel_timeout = 2_u64;
        let origin = block_info(1);
        let mut cursor = PipelineCursor::new(channel_timeout, origin);
        // capacity = 2 + 5 = 7; fill it completely
        let capacity = cursor.capacity;

        for i in 2..=(capacity as u64 + 1) {
            cursor.advance(block_info(i), tip_cursor(i * 10));
        }

        // tips map should never exceed capacity
        assert!(cursor.tips.len() <= capacity);
        // oldest origin (block 1) should have been evicted
        assert!(!cursor.origins.contains(&1));
        assert!(!cursor.tips.contains_key(&1));
    }

    #[test]
    fn reset_to_exact_channel_start_returns_correct_tip() {
        let channel_timeout = 3_u64;
        let origin = block_info(1);
        let mut cursor = PipelineCursor::new(channel_timeout, origin);

        // advance through blocks 2..=8
        for i in 2..=8_u64 {
            cursor.advance(block_info(i), tip_cursor(i * 10));
        }

        // fork at block 7 → channel_start = 7 - 3 = 4
        let (returned_tip, returned_origin) = cursor.reset(7);
        assert_eq!(returned_origin.number, 4);
        assert_eq!(returned_tip.l2_safe_head.block_info.number, 40);
    }

    #[test]
    fn reset_falls_back_to_closest_known_block_when_exact_missing() {
        let channel_timeout = 10_u64;
        let origin = block_info(100);
        let mut cursor = PipelineCursor::new(channel_timeout, origin);

        // advance only to block 105 — blocks 90..99 are not in cache
        for i in 101..=105_u64 {
            cursor.advance(block_info(i), tip_cursor(i));
        }

        // fork at 110 → channel_start = 110 - 10 = 100, which IS in the cache
        let (returned_tip, returned_origin) = cursor.reset(110);
        assert_eq!(returned_origin.number, 100);
        assert_eq!(returned_tip.l2_safe_head.block_info.number, 100);
    }

    #[test]
    fn l2_safe_head_header_returns_correct_header() {
        let origin = block_info(1);
        let mut cursor = PipelineCursor::new(5, origin);
        let tip = tip_cursor(7);
        cursor.advance(block_info(2), tip);

        assert_eq!(cursor.l2_safe_head_header().number, 7);
    }

    #[test]
    fn l2_safe_head_output_root_matches_tip() {
        let origin = block_info(1);
        let mut cursor = PipelineCursor::new(5, origin);
        let tip = tip_cursor(9);
        let expected_root = B256::repeat_byte(9);
        cursor.advance(block_info(2), tip);

        assert_eq!(*cursor.l2_safe_head_output_root(), expected_root);
    }
}
