//! Sync target with trie data cache for the background sync loop.
//!
//! Buffers trie data from exex notifications so the sync loop can use
//! pre-computed data even when it is many blocks behind the chain tip.
//! Routes committed, reverted, and reorged notifications through a
//! [`SyncTargetState`] state machine so the sync loop is the single
//! writer to proofs storage.

use std::{collections::BTreeMap, sync::Mutex};

use alloy_eips::eip1898::BlockWithParent;
use reth_trie::LazyTrieData;
use tokio::sync::Notify;
use tracing::debug;

/// Maximum number of blocks to cache trie data for.
const CACHE_CAPACITY: usize = 1024;

/// Cached trie data for a single block.
#[derive(Debug)]
pub struct CachedBlockTrieData {
    /// The block identifier with its parent hash.
    pub block_with_parent: BlockWithParent,
    /// The lazy trie data (hashed state + trie updates).
    pub trie_data: LazyTrieData,
}

/// The state of the sync target, describing what the sync loop should do next.
#[derive(Debug)]
pub enum SyncTargetState {
    /// Sync forward to a specific block number.
    SyncUpTo {
        /// The target block number to sync up to.
        to: u64,
    },
    /// Revert to a specific block, then sync forward to a target.
    RevertThenSync {
        /// The first block to remove (inclusive) during the revert.
        revert_to: BlockWithParent,
        /// The target block number to sync up to after reverting.
        sync_to: u64,
    },
    /// Revert to a specific block without syncing forward.
    Revert {
        /// The first block to remove (inclusive) during the revert.
        revert_to: BlockWithParent,
    },
}

impl SyncTargetState {
    const fn apply_next(&mut self, new: Self) {
        *self = match (&*self, new) {
            // If we are just syncing to tip already, replace with the new state.
            (Self::SyncUpTo { .. }, new) => new,

            // If the new state is a revert, replace with the new state.
            (_, Self::Revert { revert_to }) => Self::Revert { revert_to },
            (_, Self::RevertThenSync { revert_to, sync_to }) => {
                Self::RevertThenSync { revert_to, sync_to }
            }

            // If we're currently reverting, replace the sync to value with the new
            // state.
            (
                Self::RevertThenSync { revert_to, .. } | Self::Revert { revert_to },
                Self::SyncUpTo { to },
            ) => Self::RevertThenSync { revert_to: *revert_to, sync_to: to },
        };
    }
}

/// Sync target that buffers trie data from recent exex notifications.
///
/// Routes all notification types (committed, reverted, reorged) through a
/// [`SyncTargetState`] state machine so the sync loop is the single writer
/// to proofs storage. Uses a [`Notify`] to wake the sync loop when new
/// state is available.
///
/// Trie data is accumulated in a bounded [`BTreeMap`] so the sync loop
/// can still use pre-computed trie data for blocks from earlier notifications.
pub struct SyncTarget {
    cache: Mutex<BTreeMap<u64, CachedBlockTrieData>>,
    state: Mutex<Option<SyncTargetState>>,
    notify: Notify,
}

impl Default for SyncTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncTarget {
    /// Create a new `SyncTarget` with no cached data and no pending state.
    pub fn new() -> Self {
        Self { cache: Mutex::new(BTreeMap::new()), state: Mutex::new(None), notify: Notify::new() }
    }

    /// Update the sync target state and wake the sync loop.
    ///
    /// If there is already a pending state, the new state is merged using
    /// [`SyncTargetState::apply_next`].
    pub fn update_state(&self, new: SyncTargetState) {
        let mut state = self.state.lock().expect("SyncTarget lock poisoned");
        match state.as_mut() {
            Some(current) => current.apply_next(new),
            None => *state = Some(new),
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Take the current pending state, leaving `None` in its place.
    ///
    /// Used by the sync loop to consume the next action to perform.
    pub fn take_state(&self) -> Option<SyncTargetState> {
        self.state.lock().expect("SyncTarget lock poisoned").take()
    }

    /// Notify the sync target that a revert has been processed up to the given block.
    ///
    /// Only strips the revert portion if the pending revert target is at or above
    /// `reverted_to` (i.e. already covered by the completed revert). If a deeper
    /// revert arrived while processing, the pending state is left unchanged.
    ///
    /// - Covered `RevertThenSync` → `SyncUpTo` (keeps the sync target)
    /// - Covered `Revert` → `None`
    /// - Deeper revert or other states → unchanged
    pub fn mark_revert_complete(&self, reverted_to: &BlockWithParent) {
        let mut state = self.state.lock().expect("SyncTarget lock poisoned");
        match &*state {
            Some(SyncTargetState::RevertThenSync { revert_to, sync_to })
                if revert_to.block.number >= reverted_to.block.number =>
            {
                *state = Some(SyncTargetState::SyncUpTo { to: *sync_to });
            }
            Some(SyncTargetState::Revert { revert_to })
                if revert_to.block.number >= reverted_to.block.number =>
            {
                *state = None;
            }
            _ => {}
        }
    }

    /// Check if there is a pending state without consuming it.
    ///
    /// Used by the sync loop to interrupt forward sync when a higher-priority
    /// state (e.g. revert) arrives.
    pub fn has_pending_state(&self) -> bool {
        self.state.lock().expect("SyncTarget lock poisoned").is_some()
    }

    /// Wait for a state change notification.
    ///
    /// Returns immediately if a notification arrived since the last call.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// Insert cached trie data for a block.
    ///
    /// Evicts the oldest entries when the cache exceeds capacity.
    pub fn insert(&self, block_number: u64, data: CachedBlockTrieData) {
        let mut cache = self.cache.lock().expect("SyncTarget lock poisoned");
        cache.insert(block_number, data);
        let mut evicted = 0u64;
        while cache.len() > CACHE_CAPACITY {
            cache.pop_first();
            evicted += 1;
        }
        if evicted > 0 {
            debug!(
                target: "base::exex::sync_target",
                block_number,
                evicted,
                "Cache full, evicted oldest entries"
            );
        }
        debug!(
            target: "base::exex::sync_target",
            block_number,
            cached_blocks = cache.len(),
            "Cached trie data for block"
        );
    }

    /// Take cached trie data for a specific block, removing it from the cache.
    pub fn take(&self, block_number: u64) -> Option<CachedBlockTrieData> {
        let result = self.cache.lock().expect("SyncTarget lock poisoned").remove(&block_number);
        if result.is_some() {
            debug!(
                target: "base::exex::sync_target",
                block_number,
                "Cache hit: trie data found for block"
            );
        } else {
            debug!(
                target: "base::exex::sync_target",
                block_number,
                "Cache miss: no trie data for block, will re-execute"
            );
        }
        result
    }

    /// Remove all cached entries at or above the given block number.
    ///
    /// Used when a revert or reorg invalidates cached blocks.
    pub fn clear_from(&self, block_number: u64) {
        let mut cache = self.cache.lock().expect("SyncTarget lock poisoned");
        let removed = cache.split_off(&block_number);
        if !removed.is_empty() {
            debug!(
                target: "base::exex::sync_target",
                block_number,
                cleared = removed.len(),
                "Cleared cached entries from block onward"
            );
        }
    }
}

impl std::fmt::Debug for SyncTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncTarget")
            .field(
                "has_pending_state",
                &self.state.lock().expect("SyncTarget lock poisoned").is_some(),
            )
            .field("cached_blocks", &self.cache.lock().expect("SyncTarget lock poisoned").len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::private::alloy_primitives::B256;
    use alloy_eips::{NumHash, eip1898::BlockWithParent};
    use reth_trie::{HashedPostStateSorted, LazyTrieData, updates::TrieUpdatesSorted};

    use super::*;

    fn b256(byte: u8) -> B256 {
        B256::new([byte; 32])
    }

    fn block_with_parent(num: u64) -> BlockWithParent {
        BlockWithParent::new(b256(num.wrapping_sub(1) as u8), NumHash::new(num, b256(num as u8)))
    }

    fn dummy_cached_data(num: u64) -> CachedBlockTrieData {
        CachedBlockTrieData {
            block_with_parent: block_with_parent(num),
            trie_data: LazyTrieData::ready(
                Arc::new(HashedPostStateSorted::default()),
                Arc::new(TrieUpdatesSorted::default()),
            ),
        }
    }

    // ---- SyncTargetState::apply_next tests ----

    #[test]
    fn apply_next_sync_replaced_by_sync() {
        let mut state = SyncTargetState::SyncUpTo { to: 10 };
        state.apply_next(SyncTargetState::SyncUpTo { to: 20 });
        assert!(matches!(state, SyncTargetState::SyncUpTo { to: 20 }));
    }

    #[test]
    fn apply_next_sync_replaced_by_revert() {
        let mut state = SyncTargetState::SyncUpTo { to: 10 };
        let revert_to = block_with_parent(5);
        state.apply_next(SyncTargetState::Revert { revert_to });
        assert!(
            matches!(state, SyncTargetState::Revert { revert_to } if revert_to.block.number == 5)
        );
    }

    #[test]
    fn apply_next_sync_replaced_by_revert_then_sync() {
        let mut state = SyncTargetState::SyncUpTo { to: 10 };
        let revert_to = block_with_parent(5);
        state.apply_next(SyncTargetState::RevertThenSync { revert_to, sync_to: 20 });
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 20 }
            if revert_to.block.number == 5
        ));
    }

    #[test]
    fn apply_next_revert_then_sync_keeps_revert_on_new_sync() {
        let revert_to = block_with_parent(5);
        let mut state = SyncTargetState::RevertThenSync { revert_to, sync_to: 10 };
        state.apply_next(SyncTargetState::SyncUpTo { to: 30 });
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 30 }
            if revert_to.block.number == 5
        ));
    }

    #[test]
    fn apply_next_revert_then_sync_replaced_by_new_revert() {
        let revert_to = block_with_parent(5);
        let mut state = SyncTargetState::RevertThenSync { revert_to, sync_to: 10 };
        let new_revert = block_with_parent(3);
        state.apply_next(SyncTargetState::Revert { revert_to: new_revert });
        assert!(matches!(
            state,
            SyncTargetState::Revert { revert_to } if revert_to.block.number == 3
        ));
    }

    #[test]
    fn apply_next_revert_gains_sync_target() {
        let revert_to = block_with_parent(5);
        let mut state = SyncTargetState::Revert { revert_to };
        state.apply_next(SyncTargetState::SyncUpTo { to: 15 });
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 15 }
            if revert_to.block.number == 5
        ));
    }

    #[test]
    fn apply_next_revert_replaced_by_revert_then_sync() {
        let revert_to = block_with_parent(5);
        let mut state = SyncTargetState::Revert { revert_to };
        let new_revert = block_with_parent(3);
        state.apply_next(SyncTargetState::RevertThenSync { revert_to: new_revert, sync_to: 20 });
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 20 }
            if revert_to.block.number == 3
        ));
    }

    // ---- SyncTarget state management tests ----

    #[test]
    fn new_sync_target_has_no_state() {
        let target = SyncTarget::new();
        assert!(!target.has_pending_state());
        assert!(target.take_state().is_none());
    }

    #[test]
    fn update_state_sets_pending() {
        let target = SyncTarget::new();
        target.update_state(SyncTargetState::SyncUpTo { to: 10 });
        assert!(target.has_pending_state());
    }

    #[test]
    fn take_state_clears_pending() {
        let target = SyncTarget::new();
        target.update_state(SyncTargetState::SyncUpTo { to: 10 });
        let state = target.take_state();
        assert!(matches!(state, Some(SyncTargetState::SyncUpTo { to: 10 })));
        assert!(!target.has_pending_state());
    }

    #[test]
    fn update_state_merges_with_existing() {
        let target = SyncTarget::new();
        let revert_to = block_with_parent(5);
        target.update_state(SyncTargetState::Revert { revert_to });
        target.update_state(SyncTargetState::SyncUpTo { to: 20 });

        let state = target.take_state().expect("should have state");
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 20 }
            if revert_to.block.number == 5
        ));
    }

    // ---- mark_revert_complete tests ----

    #[test]
    fn mark_revert_complete_clears_covered_revert() {
        let target = SyncTarget::new();
        let revert_to = block_with_parent(5);
        target.update_state(SyncTargetState::Revert { revert_to });

        target.mark_revert_complete(&block_with_parent(5));
        assert!(!target.has_pending_state());
    }

    #[test]
    fn mark_revert_complete_strips_covered_revert_then_sync() {
        let target = SyncTarget::new();
        let revert_to = block_with_parent(5);
        target.update_state(SyncTargetState::RevertThenSync { revert_to, sync_to: 20 });

        target.mark_revert_complete(&block_with_parent(5));
        let state = target.take_state().expect("should have state");
        assert!(matches!(state, SyncTargetState::SyncUpTo { to: 20 }));
    }

    #[test]
    fn mark_revert_complete_keeps_deeper_revert() {
        let target = SyncTarget::new();
        // A deeper revert (block 3) arrived while we were reverting to block 5
        let revert_to = block_with_parent(3);
        target.update_state(SyncTargetState::Revert { revert_to });

        target.mark_revert_complete(&block_with_parent(5));
        let state = target.take_state().expect("should still have state");
        assert!(matches!(
            state,
            SyncTargetState::Revert { revert_to } if revert_to.block.number == 3
        ));
    }

    #[test]
    fn mark_revert_complete_keeps_deeper_revert_then_sync() {
        let target = SyncTarget::new();
        let revert_to = block_with_parent(3);
        target.update_state(SyncTargetState::RevertThenSync { revert_to, sync_to: 20 });

        target.mark_revert_complete(&block_with_parent(5));
        let state = target.take_state().expect("should still have state");
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 20 }
            if revert_to.block.number == 3
        ));
    }

    #[test]
    fn mark_revert_complete_clears_shallower_revert() {
        let target = SyncTarget::new();
        // Pending revert to block 8 is shallower than what we reverted to (block 5)
        let revert_to = block_with_parent(8);
        target.update_state(SyncTargetState::Revert { revert_to });

        target.mark_revert_complete(&block_with_parent(5));
        assert!(!target.has_pending_state());
    }

    #[test]
    fn mark_revert_complete_noop_on_sync_up_to() {
        let target = SyncTarget::new();
        target.update_state(SyncTargetState::SyncUpTo { to: 20 });

        target.mark_revert_complete(&block_with_parent(5));
        let state = target.take_state().expect("should still have state");
        assert!(matches!(state, SyncTargetState::SyncUpTo { to: 20 }));
    }

    #[test]
    fn mark_revert_complete_noop_on_empty() {
        let target = SyncTarget::new();
        target.mark_revert_complete(&block_with_parent(5));
        assert!(!target.has_pending_state());
    }

    // ---- Cache tests ----

    #[test]
    fn insert_and_take_cached_data() {
        let target = SyncTarget::new();
        target.insert(42, dummy_cached_data(42));

        let taken = target.take(42);
        assert!(taken.is_some());
        assert_eq!(taken.unwrap().block_with_parent.block.number, 42);

        // Second take returns None (was removed)
        assert!(target.take(42).is_none());
    }

    #[test]
    fn take_nonexistent_block_returns_none() {
        let target = SyncTarget::new();
        assert!(target.take(999).is_none());
    }

    #[test]
    fn clear_from_removes_blocks_at_and_above() {
        let target = SyncTarget::new();
        for n in 1..=10 {
            target.insert(n, dummy_cached_data(n));
        }

        target.clear_from(6);

        // Blocks 1..=5 remain
        for n in 1..=5 {
            assert!(target.take(n).is_some(), "block {n} should still be cached");
        }
        // Blocks 6..=10 removed
        for n in 6..=10 {
            assert!(target.take(n).is_none(), "block {n} should have been cleared");
        }
    }

    #[test]
    fn clear_from_at_start_clears_everything() {
        let target = SyncTarget::new();
        for n in 5..=10 {
            target.insert(n, dummy_cached_data(n));
        }
        target.clear_from(5);
        for n in 5..=10 {
            assert!(target.take(n).is_none());
        }
    }

    #[test]
    fn clear_from_beyond_cache_is_noop() {
        let target = SyncTarget::new();
        for n in 1..=5 {
            target.insert(n, dummy_cached_data(n));
        }
        target.clear_from(100);
        for n in 1..=5 {
            assert!(target.take(n).is_some());
        }
    }

    #[test]
    fn cache_evicts_oldest_when_full() {
        let target = SyncTarget::new();

        // Fill to capacity + 10
        for n in 1..=(CACHE_CAPACITY as u64 + 10) {
            target.insert(n, dummy_cached_data(n));
        }

        // Oldest 10 blocks should have been evicted
        for n in 1..=10 {
            assert!(target.take(n).is_none(), "block {n} should have been evicted");
        }

        // Block 11 onward should still be present
        assert!(target.take(11).is_some());
        assert!(target.take(CACHE_CAPACITY as u64 + 10).is_some());
    }

    #[test]
    fn default_creates_empty_sync_target() {
        let target = SyncTarget::default();
        assert!(!target.has_pending_state());
        assert!(target.take(0).is_none());
    }

    #[test]
    fn debug_impl_does_not_panic() {
        let target = SyncTarget::new();
        target.insert(1, dummy_cached_data(1));
        target.update_state(SyncTargetState::SyncUpTo { to: 5 });
        let debug_str = format!("{target:?}");
        assert!(debug_str.contains("SyncTarget"));
        assert!(debug_str.contains("has_pending_state"));
        assert!(debug_str.contains("cached_blocks"));
    }

    #[tokio::test]
    async fn notified_wakes_on_update_state() {
        let target = Arc::new(SyncTarget::new());
        let target_clone = Arc::clone(&target);

        // Spawn a task that waits for notification
        let handle = tokio::spawn(async move {
            target_clone.notified().await;
            target_clone.take_state()
        });

        // Give the task time to start waiting
        tokio::task::yield_now().await;

        target.update_state(SyncTargetState::SyncUpTo { to: 42 });

        let result = handle.await.expect("task should complete");
        assert!(matches!(result, Some(SyncTargetState::SyncUpTo { to: 42 })));
    }
}
