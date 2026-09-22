//! Bounded in-memory map of payload witnesses.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;
use alloy_rpc_types::debug::ExecutionWitness;

/// About two hours of 2-second Base blocks.
///
/// Uncompressed `debug_executePayload` responses are about 15 megabytes, so retaining
/// this many witnesses is on the order of 50 gigabytes of RAM.
pub const DEFAULT_WITNESS_CACHE_BLOCKS: usize = 3600;

/// Lookup key for one cached payload witness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WitnessKey {
    /// Parent block hash passed to `debug_executePayload`.
    pub parent_hash: B256,
    /// [`crate::PayloadAttributes::digest`] of the request attributes.
    pub attributes_digest: B256,
}

struct Entry {
    witness: Arc<ExecutionWitness>,
}

struct Inner {
    max_blocks: usize,
    entries: HashMap<WitnessKey, Entry>,
    order: VecDeque<WitnessKey>,
}

/// Ring of payload witnesses. The oldest insert is dropped once `max_blocks` is exceeded.
pub struct WitnessCache {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for WitnessCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("WitnessCache")
            .field("len", &inner.entries.len())
            .field("max_blocks", &inner.max_blocks)
            .finish()
    }
}

impl WitnessCache {
    /// Creates an empty cache that retains at most `max_blocks` witnesses.
    ///
    /// # Panics
    ///
    /// Panics if `max_blocks` is zero.
    pub fn new(max_blocks: usize) -> Self {
        assert!(max_blocks > 0, "witness cache must retain at least one block");
        Self {
            inner: Mutex::new(Inner {
                max_blocks,
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Stores `witness` under `key`.
    ///
    /// Re-inserting a key moves it to the newest position so it stays evictable and is dropped
    /// only after older entries.
    pub fn insert(&self, key: WitnessKey, witness: ExecutionWitness) {
        let mut inner = self.lock();
        if inner.entries.contains_key(&key) {
            inner.order.retain(|existing| existing != &key);
        }
        inner.order.push_back(key);
        inner.entries.insert(key, Entry { witness: Arc::new(witness) });
        while inner.entries.len() > inner.max_blocks {
            let Some(evicted) = inner.order.pop_front() else {
                break;
            };
            inner.entries.remove(&evicted);
        }
    }

    /// Returns the witness stored for `key`.
    pub fn get(&self, key: WitnessKey) -> Option<Arc<ExecutionWitness>> {
        self.lock().entries.get(&key).map(|entry| Arc::clone(&entry.witness))
    }

    /// Returns the number of retained witnesses.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Returns true when no witnesses are retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> WitnessKey {
        WitnessKey {
            parent_hash: B256::repeat_byte(byte),
            attributes_digest: B256::repeat_byte(byte),
        }
    }

    fn witness() -> ExecutionWitness {
        serde_json::from_str(r#"{"state":[],"codes":[],"keys":[],"headers":[]}"#).unwrap()
    }

    #[test]
    fn returns_the_witness_stored_for_a_key() {
        let cache = WitnessCache::new(2);
        cache.insert(key(1), witness());

        assert!(cache.get(key(1)).is_some());
        assert!(cache.get(key(2)).is_none());
    }

    #[test]
    fn evicts_the_oldest_witness_past_capacity() {
        let cache = WitnessCache::new(2);
        cache.insert(key(1), witness());
        cache.insert(key(2), witness());
        cache.insert(key(3), witness());

        assert!(cache.get(key(1)).is_none());
        assert!(cache.get(key(2)).is_some());
        assert!(cache.get(key(3)).is_some());
    }
}
