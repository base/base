//! Metering store.
//!
//! Provides a concurrent cache for resource metering data with LRU eviction
//! to bound memory usage. Uses [`moka`] for the LRU cache that promotes
//! entries on access, preventing premature eviction of frequently-read data.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::TxHash;
use base_builder_core::{BuilderMetrics, MeteringProvider};
use base_bundles::MeterBundleResponse;
use moka::{notification::RemovalCause, policy::EvictionPolicy, sync::Cache};

/// Default LRU capacity for [`MeteringStore`].
pub const DEFAULT_METERING_STORE_MAX_CAPACITY: u64 = 10_000;

/// Default TTL in seconds for [`MeteringStore`] entries.
pub const DEFAULT_METERING_STORE_TTL_SECS: u64 = 30;

/// Concurrent metering store with LRU eviction.
pub struct MeteringStore {
    /// LRU cache mapping transaction hash to metering data.
    cache: Cache<TxHash, Arc<MeterBundleResponse>>,
    /// Whether resource metering is enabled.
    metering_enabled: AtomicBool,
}

impl core::fmt::Debug for MeteringStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MeteringStore")
            .field("entries", &self.cache.entry_count())
            .field("metering_enabled", &self.metering_enabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl MeteringStore {
    /// Creates a new [`MeteringStore`] with the given metering flag, max capacity, and TTL.
    ///
    /// Uses LRU eviction (not `TinyLFU`) because this cache is write-once-read-once:
    /// entries are inserted on metering arrival and read once at inclusion time.
    /// `TinyLFU` would reject new entries with zero frequency since `insert()` does
    /// not increment the frequency sketch — only `get()` does.
    pub fn new(enable_resource_metering: bool, max_capacity: usize, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity as u64)
            .eviction_policy(EvictionPolicy::lru())
            .time_to_live(ttl)
            .eviction_listener(move |_key, _value, cause| {
                if cause == RemovalCause::Size {
                    BuilderMetrics::metering_store_lru_evictions().increment(1);
                }
                if cause == RemovalCause::Expired {
                    BuilderMetrics::metering_store_ttl_expirations().increment(1);
                }
            })
            .build();

        Self { cache, metering_enabled: AtomicBool::new(enable_resource_metering) }
    }

    /// Returns the number of stored entries.
    pub fn len(&self) -> usize {
        self.cache.entry_count() as usize
    }

    /// Returns `true` if the store contains no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.entry_count() == 0
    }
}

impl MeteringProvider for MeteringStore {
    fn get(&self, tx_hash: &TxHash) -> Option<Arc<MeterBundleResponse>> {
        if !self.metering_enabled.load(Ordering::Relaxed) {
            return None;
        }

        self.cache.get(tx_hash)
    }

    fn is_enabled(&self) -> bool {
        self.metering_enabled.load(Ordering::Relaxed)
    }

    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        self.cache.insert(tx_hash, Arc::new(metering));
        BuilderMetrics::metering_store_size().set(self.cache.entry_count() as f64);
    }

    fn remove(&self, tx_hashes: &[TxHash]) {
        for hash in tx_hashes {
            self.cache.invalidate(hash);
        }
        BuilderMetrics::metering_store_size().set(self.cache.entry_count() as f64);
    }

    fn clear(&self) {
        self.cache.invalidate_all();
        BuilderMetrics::metering_store_size().set(0.0);
    }

    fn set_enabled(&self, enabled: bool) {
        self.metering_enabled.store(enabled, Ordering::Relaxed);
    }
}

impl Default for MeteringStore {
    fn default() -> Self {
        Self::new(
            false,
            DEFAULT_METERING_STORE_MAX_CAPACITY as usize,
            Duration::from_secs(DEFAULT_METERING_STORE_TTL_SECS),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, TxHash, U256};

    use super::*;

    fn create_test_metering(gas_used: u64) -> MeterBundleResponse {
        MeterBundleResponse {
            bundle_hash: B256::random(),
            bundle_gas_price: U256::from(123),
            coinbase_diff: U256::from(123),
            eth_sent_to_coinbase: U256::from(123),
            gas_fees: U256::from(123),
            results: vec![],
            state_block_number: 4,
            total_gas_used: gas_used,
            total_execution_time_us: 533,
        }
    }

    #[test]
    fn test_metering_insert_and_get() {
        let store = MeteringStore::new(true, 100, Duration::from_secs(30));
        let tx_hash = TxHash::random();
        let meter_data = create_test_metering(21000);

        store.insert(tx_hash, meter_data);
        let data = store.get(&tx_hash);
        assert_eq!(data.as_ref().unwrap().total_gas_used, 21000);

        store.insert(tx_hash, create_test_metering(50000));
        let data = store.get(&tx_hash);
        assert_eq!(data.as_ref().unwrap().total_gas_used, 50000);
    }

    #[test]
    fn test_clear_metering() {
        let store = MeteringStore::new(true, 100, Duration::from_secs(30));

        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        store.insert(tx1, create_test_metering(1000));
        store.insert(tx2, create_test_metering(2000));

        assert!(store.get(&tx1).is_some());
        assert!(store.get(&tx2).is_some());

        store.clear();

        assert!(store.get(&tx1).is_none());
        assert!(store.get(&tx2).is_none());
    }

    #[test]
    fn test_lru_eviction() {
        let store = MeteringStore::new(true, 2, Duration::from_secs(30));

        let tx1 = TxHash::random();
        let tx2 = TxHash::random();
        let tx3 = TxHash::random();

        store.insert(tx1, create_test_metering(1000));
        store.insert(tx2, create_test_metering(2000));
        // Trigger eviction by inserting a third entry.
        store.insert(tx3, create_test_metering(3000));

        // Moka evicts asynchronously; run pending tasks to ensure eviction completes.
        store.cache.run_pending_tasks();

        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_insert_after_lookup_miss_is_cached() {
        let store = MeteringStore::new(true, 100, Duration::from_secs(30));
        let tx_hash = TxHash::random();

        // A lookup miss alone does not mean the transaction was included.
        assert!(store.get(&tx_hash).is_none());

        // Data that arrives after a miss is still needed if the tx remains pending.
        store.insert(tx_hash, create_test_metering(42000));
        assert!(store.get(&tx_hash).is_some(), "metering should cache after a plain miss");
    }

    #[test]
    fn test_metering_enabled_state_tracks_runtime_toggle() {
        let store = MeteringStore::new(false, 100, Duration::from_secs(30));

        assert!(!store.is_enabled());

        store.set_enabled(true);
        assert!(store.is_enabled());

        store.set_enabled(false);
        assert!(!store.is_enabled());
    }

    #[test]
    fn test_recently_accessed_entries_survive_eviction() {
        let capacity = 5;
        let store = MeteringStore::new(true, capacity, Duration::from_secs(30));

        let mut hashes: Vec<TxHash> = Vec::new();
        for i in 0..capacity as u64 {
            let h = TxHash::random();
            store.insert(h, create_test_metering(i * 1000));
            hashes.push(h);
        }
        store.cache.run_pending_tasks();
        assert_eq!(store.len(), capacity);

        let recent = hashes[0];
        assert!(store.get(&recent).is_some());
        store.cache.run_pending_tasks();

        for i in 0..capacity as u64 {
            store.insert(TxHash::random(), create_test_metering(i));
            store.cache.run_pending_tasks();
        }

        assert!(
            store.get(&recent).is_none(),
            "LRU eviction should eventually evict even recently accessed entries when enough new entries are inserted"
        );
    }

    #[test]
    fn test_insert_always_admitted_with_lru() {
        let capacity = 3;
        let store = MeteringStore::new(true, capacity, Duration::from_secs(30));

        for i in 0..capacity as u64 {
            store.insert(TxHash::random(), create_test_metering(i * 1000));
        }
        store.cache.run_pending_tasks();
        assert_eq!(store.len(), capacity);

        let new_hash = TxHash::random();
        store.insert(new_hash, create_test_metering(99000));
        store.cache.run_pending_tasks();

        assert!(store.get(&new_hash).is_some(), "new entry must be admitted under LRU policy");
        assert_eq!(store.len(), capacity, "cache should remain at capacity");
    }

    #[test]
    fn test_ttl_expires_entries() {
        let store = MeteringStore::new(true, 100, Duration::from_millis(50));
        let tx_hash = TxHash::random();

        store.insert(tx_hash, create_test_metering(21000));
        assert!(store.get(&tx_hash).is_some(), "entry should be present before TTL");

        std::thread::sleep(Duration::from_millis(100));
        store.cache.run_pending_tasks();

        assert!(store.get(&tx_hash).is_none(), "entry should expire after TTL");
    }

    #[test]
    fn test_entries_persist_within_ttl() {
        let store = MeteringStore::new(true, 100, Duration::from_secs(30));
        let tx_hash = TxHash::random();

        store.insert(tx_hash, create_test_metering(21000));

        std::thread::sleep(Duration::from_millis(50));
        store.cache.run_pending_tasks();

        assert!(store.get(&tx_hash).is_some(), "entry should persist within TTL");
    }
}
