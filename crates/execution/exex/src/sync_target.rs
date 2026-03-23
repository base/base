//! Sync target with trie data cache for the background sync loop.
//!
//! Buffers trie data from exex notifications so the sync loop can use
//! pre-computed data even when it is many blocks behind the chain tip.

use std::{collections::BTreeMap, sync::Mutex};

use alloy_eips::eip1898::BlockWithParent;
use reth_trie::LazyTrieData;
use tokio::sync::watch;
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

/// Sync target that buffers trie data from recent exex notifications.
///
/// Uses a `watch` channel to signal target changes to the sync loop. Unlike
/// `Notify`, `watch::Receiver::changed()` will always detect updates that
/// occurred since the last call — even if the receiver was busy processing
/// blocks at the time of the update. This eliminates the race condition where
/// `Notify::notify_waiters()` fires while the sync loop is not awaiting, causing
/// the notification to be permanently lost.
///
/// Trie data is accumulated in a bounded [`BTreeMap`] so the sync loop
/// can still use pre-computed trie data for blocks from earlier notifications.
pub struct SyncTarget {
    cache: Mutex<BTreeMap<u64, CachedBlockTrieData>>,
    target_tx: watch::Sender<u64>,
    target_rx: watch::Receiver<u64>,
}

impl Default for SyncTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncTarget {
    /// Create a new `SyncTarget` with no cached data and target 0.
    pub fn new() -> Self {
        let (target_tx, target_rx) = watch::channel(0u64);
        Self { cache: Mutex::new(BTreeMap::new()), target_tx, target_rx }
    }

    /// Set the sync target block number and wake the sync loop.
    ///
    /// Only advances the target forward; ignored if `target` is not greater
    /// than the current value.
    pub fn set_target(&self, target: u64) {
        let current = *self.target_tx.borrow();
        if target > current {
            debug!(
                target: "base::exex::sync_target",
                prev_target = current,
                new_target = target,
                cached_blocks = self.cache.lock().expect("SyncTarget lock poisoned").len(),
                "Sync target advanced"
            );
            // send() always succeeds while we hold the receiver.
            self.target_tx.send(target).expect("watch receiver dropped");
        }
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

    /// Get the current sync target block number.
    pub fn target(&self) -> u64 {
        *self.target_tx.borrow()
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

    /// Subscribe to target changes.
    ///
    /// Returns a `watch::Receiver` that the sync loop uses to detect new
    /// targets. Unlike `Notify`, a `watch` channel retains the latest value
    /// so no update is ever missed.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.target_rx.clone()
    }
}

impl std::fmt::Debug for SyncTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncTarget")
            .field("target", &*self.target_tx.borrow())
            .field("cached_blocks", &self.cache.lock().expect("SyncTarget lock poisoned").len())
            .finish()
    }
}
