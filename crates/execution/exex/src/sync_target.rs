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
    fn apply_next(&mut self, new: SyncTargetState) {
        *self = match (&*self, new) {
            // If we are just syncing to tip already, replace with the new state.
            (SyncTargetState::SyncUpTo { .. }, new) => new,

            // If the new state is a revert, replace with the new state.
            (_, SyncTargetState::Revert { revert_to }) => SyncTargetState::Revert { revert_to },
            (_, SyncTargetState::RevertThenSync { revert_to, sync_to }) => {
                SyncTargetState::RevertThenSync { revert_to, sync_to }
            }

            // If we're currently reverting and syncing, replace the sync to value with the new
            // state.
            (SyncTargetState::RevertThenSync { .. }, SyncTargetState::SyncUpTo { to }) => {
                SyncTargetState::SyncUpTo { to }
            }

            // If we're currently reverting without syncing, add the sync target.
            (SyncTargetState::Revert { revert_to }, SyncTargetState::SyncUpTo { to }) => {
                SyncTargetState::RevertThenSync { revert_to: *revert_to, sync_to: to }
            }
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
        Self {
            cache: Mutex::new(BTreeMap::new()),
            state: Mutex::new(None),
            notify: Notify::new(),
        }
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
