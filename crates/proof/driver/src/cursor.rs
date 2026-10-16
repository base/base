//! Contains the cursor for the derivation pipeline.
//!
//! This module provides the [`PipelineCursor`] which tracks the state of the derivation
//! pipeline, including its current L1 origin and L2 safe head.

use alloc::collections::{btree_map::BTreeMap, vec_deque::VecDeque};

use alloy_primitives::map::HashMap;
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
    /// The channel timeout in blocks used to size the initial cache capacity.
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
}
