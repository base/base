//! Metering store.
//!
//! Provides a concurrent cache for resource metering data with LRU eviction
//! to bound memory usage.

use std::sync::atomic::{AtomicBool, Ordering};

use alloy_primitives::TxHash;
use base_builder_core::{BuilderMetrics, MeteringProvider};
use base_bundles::MeterBundleResponse;
use concurrent_queue::ConcurrentQueue;
use tracing::debug;

/// Concurrent metering store with LRU eviction.
pub struct MeteringStore {
    /// Mapping of transaction hash to metering data.
    by_tx_hash: dashmap::DashMap<TxHash, MeterBundleResponse>,
    /// LRU queue for transaction hash eviction.
    lru: ConcurrentQueue<TxHash>,
    /// Whether resource metering is enabled.
    metering_enabled: AtomicBool,
    /// Builder metrics.
    metrics: BuilderMetrics,
}

impl core::fmt::Debug for MeteringStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MeteringStore")
            .field("entries", &self.by_tx_hash.len())
            .field("metering_enabled", &self.metering_enabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl MeteringStore {
    /// Creates a new [`MeteringStore`] with the given metering flag and LRU buffer size.
    pub fn new(enable_resource_metering: bool, buffer_size: usize) -> Self {
        Self {
            by_tx_hash: dashmap::DashMap::new(),
            lru: ConcurrentQueue::bounded(buffer_size),
            metering_enabled: AtomicBool::new(enable_resource_metering),
            metrics: BuilderMetrics::default(),
        }
    }

    fn evict_if_needed(&self) {
        if self.lru.is_full()
            && let Ok(evicted_hash) = self.lru.pop()
        {
            self.by_tx_hash.remove(&evicted_hash);
            debug!(
                target: "metering_store",
                evicted_tx = ?evicted_hash,
                "Evicted old metering data"
            );
        }
    }

    /// Returns the number of stored entries.
    pub fn len(&self) -> usize {
        self.by_tx_hash.len()
    }

    /// Returns `true` if the store contains no entries.
    pub fn is_empty(&self) -> bool {
        self.by_tx_hash.is_empty()
    }
}

impl MeteringProvider for MeteringStore {
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        if !self.metering_enabled.load(Ordering::Relaxed) {
            return None;
        }

        let Some(entry) = self.by_tx_hash.get(tx_hash) else {
            self.metrics.metering_unknown_transaction.increment(1);
            return None;
        };

        self.metrics.metering_known_transaction.increment(1);
        Some(entry.clone())
    }

    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        self.evict_if_needed();
        let _ = self.lru.push(tx_hash);
        self.by_tx_hash.insert(tx_hash, metering);
    }

    fn clear(&self) {
        self.by_tx_hash.clear();
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

        for i in 0..3u64 {
            let tx_hash = TxHash::random();
            store.insert(tx_hash, create_test_metering(i * 1000));
        }

        assert_eq!(store.len(), 2);
    }
}
