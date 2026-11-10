//! Contains the cursor for the derivation pipeline.
//!
//! This module provides the [`PipelineCursor`] which tracks the state of the derivation
//! pipeline, including its current L1 origin and L2 safe head.

use alloc::collections::{btree_map::BTreeMap, vec_deque::VecDeque};

use base_protocol::{BlockInfo, L2BlockInfo};

use crate::TipCursor;

/// A cursor that tracks the derivation pipeline state and progress.
///
/// The [`PipelineCursor`] maintains a capacity-bounded map of recent L1 origins to
/// their corresponding L2 tips.
#[derive(Debug, Clone)]
pub struct PipelineCursor {
    /// The maximum number of cached L1/L2 mappings before evicting old entries.
    pub capacity: usize,
    /// The current L1 origin block that the pipeline is processing.
    pub origin: BlockInfo,
    /// Keys of `tips` in insertion order, oldest first, for cache eviction.
    pub origins: VecDeque<u64>,
    /// Mapping from L1 origin block numbers to their corresponding L2 tips.
    pub tips: BTreeMap<u64, TipCursor>,
}

impl PipelineCursor {
    /// Creates a new pipeline cursor with the specified channel timeout and initial origin.
    pub fn new(channel_timeout: u64, origin: BlockInfo) -> Self {
        let capacity = channel_timeout as usize + 5;
        Self {
            capacity,
            origin,
            origins: VecDeque::with_capacity(capacity),
            tips: Default::default(),
        }
    }

    /// Returns the current L1 origin block being processed by the pipeline.
    pub const fn origin(&self) -> BlockInfo {
        self.origin
    }

    /// Returns the current L2 safe head block information.
    pub fn l2_safe_head(&self) -> &L2BlockInfo {
        &self.tip().l2_safe_head
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
    ///
    /// Repeated calls with the same L1 origin replace that origin's tip. Only a new origin can
    /// evict the oldest one, so at most `capacity` origins are cached.
    pub fn advance(&mut self, origin: BlockInfo, l2_tip_block: TipCursor) {
        if !self.tips.contains_key(&origin.number) {
            if self.tips.len() >= self.capacity
                && let Some(oldest) = self.origins.pop_front()
            {
                self.tips.remove(&oldest);
            }
            self.origins.push_back(origin.number);
        }

        self.origin = origin;
        self.tips.insert(origin.number, l2_tip_block);
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::B256;

    use super::*;

    fn l1_block(number: u64) -> BlockInfo {
        BlockInfo { number, ..Default::default() }
    }

    fn tip(l2_number: u64) -> TipCursor {
        let mut l2_safe_head = L2BlockInfo::default();
        l2_safe_head.block_info.number = l2_number;
        TipCursor::new(l2_safe_head, Sealed::new(Header::default()), B256::ZERO)
    }

    #[test]
    fn many_l2_blocks_per_l1_origin_stay_within_capacity() {
        let mut cursor = PipelineCursor::new(0, l1_block(0));
        let capacity = cursor.capacity;
        let blocks_per_origin = 10;

        let mut l2_number = 0;
        for l1_number in 0..(capacity as u64 * 3) {
            for _ in 0..blocks_per_origin {
                cursor.advance(l1_block(l1_number), tip(l2_number));
                l2_number += 1;
            }
            assert!(cursor.tips.len() <= capacity);
            assert!(cursor.origins.len() <= capacity);
        }

        let last_l1 = capacity as u64 * 3 - 1;
        assert_eq!(cursor.origin().number, last_l1);
        assert_eq!(cursor.l2_safe_head().block_info.number, l2_number - 1);
        assert_eq!(
            cursor.tips.keys().copied().collect::<Vec<_>>(),
            (last_l1 + 1 - capacity as u64..=last_l1).collect::<Vec<_>>()
        );
    }
}
