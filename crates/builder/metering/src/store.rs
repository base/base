//! Metering store.
//!
//! Provides a concurrent cache for resource metering data with LRU eviction
//! to bound memory usage. Uses [`moka`] for the LRU cache that promotes
//! entries on access, preventing premature eviction of frequently-read data.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use alloy_primitives::TxHash;
use base_builder_core::{BuilderMetrics, MeteringProvider};
use base_bundles::MeterBundleResponse;
use moka::{notification::RemovalCause, sync::Cache};

/// Concurrent metering store with LRU eviction.
pub struct MeteringStore {
    /// LRU cache mapping transaction hash to metering data.
    cache: Cache<TxHash, MeterBundleResponse>,
    /// Records when `get()` first returned `None` for a tx hash — the moment
    /// the builder needed metering data but didn't have it. Cleared when the
    /// tx is skipped (`MeteringDataPending`) so only txs that were actually
    /// included without data retain their entry for late-arrival detection.
    needed_at: Cache<TxHash, Instant>,
    /// Whether resource metering is enabled.
    metering_enabled: AtomicBool,
}

impl core::fmt::Debug for MeteringStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MeteringStore")
            .field("entries", &self.cache.entry_count())
            .field("needed_at", &self.needed_at.entry_count())
            .field("metering_enabled", &self.metering_enabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl MeteringStore {
    /// Creates a new [`MeteringStore`] with the given metering flag and max capacity.
    pub fn new(enable_resource_metering: bool, max_capacity: usize) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity as u64)
            .eviction_listener(move |_key, _value, cause| {
                if cause == RemovalCause::Size {
                    BuilderMetrics::metering_store_lru_evictions().increment(1);
                }
            })
            .build();

        let needed_at = Cache::builder().max_capacity(max_capacity as u64).build();

        Self { cache, needed_at, metering_enabled: AtomicBool::new(enable_resource_metering) }
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
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        if !self.metering_enabled.load(Ordering::Relaxed) {
            return None;
        }

        let Some(entry) = self.cache.get(tx_hash) else {
            // Atomically record the first miss — later flashblock iterations
            // must not overwrite the original timestamp.
            self.needed_at.entry_by_ref(tx_hash).or_insert(Instant::now());
            return None;
        };

        Some(entry)
    }

    fn is_enabled(&self) -> bool {
        self.metering_enabled.load(Ordering::Relaxed)
    }

    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        // If the builder needed metering data for this tx but didn't have it,
        // the data arrived late. Record how late and what the values were.
        if let Some(needed_at) = self.needed_at.remove(&tx_hash) {
            let latency_ms = needed_at.elapsed().as_millis() as f64;
            BuilderMetrics::metering_late_arrival_total().increment(1);
            BuilderMetrics::metering_late_arrival_latency_ms().record(latency_ms);
            BuilderMetrics::metering_late_arrival_execution_time_us()
                .record(metering.total_execution_time_us as f64);
            BuilderMetrics::metering_late_arrival_state_root_time_us()
                .record(metering.state_root_time_us as f64);
            return;
        }

        self.cache.insert(tx_hash, metering);
        BuilderMetrics::metering_store_size().set(self.cache.entry_count() as f64);
    }

    fn skip(&self, tx_hash: &TxHash) {
        self.needed_at.invalidate(tx_hash);
    }

    fn remove(&self, tx_hashes: &[TxHash]) {
        for hash in tx_hashes {
            self.cache.invalidate(hash);
        }
        BuilderMetrics::metering_store_size().set(self.cache.entry_count() as f64);
    }

    fn clear(&self) {
        self.cache.invalidate_all();
        self.needed_at.invalidate_all();
        BuilderMetrics::metering_store_size().set(0.0);
    }

    fn set_enabled(&self, enabled: bool) {
        self.metering_enabled.store(enabled, Ordering::Relaxed);
    }
}

impl Default for MeteringStore {
    fn default() -> Self {
        Self::new(false, 10_000)
    }
}

#[cfg(test)]
impl MeteringStore {
    /// Runs pending async tasks in all caches (for deterministic tests).
    fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks();
        self.needed_at.run_pending_tasks();
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
            state_flashblock_index: None,
            total_gas_used: gas_used,
            total_execution_time_us: 533,
            state_root_time_us: 0,
            state_root_account_leaf_count: 0,
            state_root_account_branch_count: 0,
            state_root_storage_leaf_count: 0,
            state_root_storage_branch_count: 0,
        }
    }

    #[test]
    fn test_metering_insert_and_get() {
        let store = MeteringStore::new(true, 100);
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
        let store = MeteringStore::new(true, 100);

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
        let store = MeteringStore::new(true, 2);

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
    fn test_late_insert_after_inclusion() {
        let store = MeteringStore::new(true, 100);
        let tx_hash = TxHash::random();

        // get() miss → tx included without data → data arrives late
        assert!(store.get(&tx_hash).is_none());
        assert!(store.needed_at.contains_key(&tx_hash));

        // Late-arriving data is consumed by insert(), does not enter cache
        store.insert(tx_hash, create_test_metering(42000));
        store.run_pending_tasks();
        assert!(!store.needed_at.contains_key(&tx_hash));
        assert!(store.get(&tx_hash).is_none(), "late arrival should not re-enter cache");
    }

    #[test]
    fn test_skip_clears_needed_at() {
        let store = MeteringStore::new(true, 100);
        let tx_hash = TxHash::random();

        // get() miss → tx skipped (MeteringDataPending)
        assert!(store.get(&tx_hash).is_none());
        assert!(store.needed_at.contains_key(&tx_hash));

        store.skip(&tx_hash);
        assert!(!store.needed_at.contains_key(&tx_hash));

        // Data arrives after skip — normal insert, not a late arrival
        store.insert(tx_hash, create_test_metering(21000));
        assert!(store.get(&tx_hash).is_some());
    }

    #[test]
    fn test_no_needed_at_when_data_present() {
        let store = MeteringStore::new(true, 100);
        let tx_hash = TxHash::random();

        // Insert data first, then get() finds it — no needed_at entry
        store.insert(tx_hash, create_test_metering(21000));
        assert!(store.get(&tx_hash).is_some());
        assert!(!store.needed_at.contains_key(&tx_hash));
    }

    #[test]
    fn test_clear_resets_needed_at() {
        let store = MeteringStore::new(true, 100);
        let tx_hash = TxHash::random();

        assert!(store.get(&tx_hash).is_none());
        assert!(store.needed_at.contains_key(&tx_hash));

        store.clear();
        assert!(!store.needed_at.contains_key(&tx_hash));
    }

    #[test]
    fn test_metering_enabled_state_tracks_runtime_toggle() {
        let store = MeteringStore::new(false, 100);

        assert!(!store.is_enabled());

        store.set_enabled(true);
        assert!(store.is_enabled());

        store.set_enabled(false);
        assert!(!store.is_enabled());
    }

    #[test]
    fn test_accessed_entries_survive_eviction() {
        // Small capacity to keep the TinyLFU frequency sketch deterministic.
        let capacity = 5;
        let store = MeteringStore::new(true, capacity);

        // Fill the cache to capacity.
        let mut hashes: Vec<TxHash> = Vec::new();
        for i in 0..capacity as u64 {
            let h = TxHash::random();
            store.insert(h, create_test_metering(i * 1000));
            hashes.push(h);
        }
        store.cache.run_pending_tasks();
        assert_eq!(store.len(), capacity);

        // Access the first entry many times to build up its frequency estimate
        // so TinyLFU's admission policy keeps it over newcomers.
        let promoted = hashes[0];
        for _ in 0..20 {
            assert!(store.get(&promoted).is_some());
        }
        store.cache.run_pending_tasks();

        // Insert new entries one at a time, flushing between each so the
        // eviction policy processes each displacement individually.
        for i in 0..capacity as u64 {
            store.insert(TxHash::random(), create_test_metering(i));
            store.cache.run_pending_tasks();
        }

        assert!(
            store.get(&promoted).is_some(),
            "frequently accessed entry should survive eviction"
        );
    }
}
