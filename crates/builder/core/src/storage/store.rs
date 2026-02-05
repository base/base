//! Transaction data store.
//!
//! Provides a concurrent cache for transaction metadata including resource metering
//! information. Uses LRU eviction to bound memory usage.

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use concurrent_queue::ConcurrentQueue;
use tracing::debug;

use super::{StoreData, TxData};
use crate::BuilderMetrics;

#[derive(Clone)]
pub struct TxDataStore {
    data: Arc<StoreData>,
    metrics: BuilderMetrics,
}

impl Debug for TxDataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxDataStore")
            .field("entries", &self.data.by_tx_hash.len())
            .field("metering_enabled", &self.data.metering_enabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl TxDataStore {
    pub fn new(enable_resource_metering: bool, buffer_size: usize) -> Self {
        Self {
            data: Arc::new(StoreData {
                by_tx_hash: dashmap::DashMap::new(),
                lru: ConcurrentQueue::bounded(buffer_size),
                metering_enabled: AtomicBool::new(enable_resource_metering),
            }),
            metrics: BuilderMetrics::default(),
        }
    }

    fn evict_if_needed(&self) {
        if self.data.lru.is_full()
            && let Ok(evicted_hash) = self.data.lru.pop()
        {
            self.data.by_tx_hash.remove(&evicted_hash);
            debug!(
                target: "tx_data_store",
                evicted_tx = ?evicted_hash,
                "Evicted old transaction data"
            );
        }
    }

    pub fn get(&self, tx_hash: &TxHash) -> TxData {
        let metering_enabled = self.data.metering_enabled.load(Ordering::Relaxed);

        let Some(entry) = self.data.by_tx_hash.get(tx_hash) else {
            if metering_enabled {
                self.metrics.metering_unknown_transaction.increment(1);
            }
            return TxData { metering: None };
        };

        let data = entry.clone();

        if metering_enabled {
            if data.metering.is_some() {
                self.metrics.metering_known_transaction.increment(1);
            } else {
                self.metrics.metering_unknown_transaction.increment(1);
            }
        }

        data
    }

    pub fn insert_metering(&self, tx_hash: TxHash, metering_info: MeterBundleResponse) {
        self.evict_if_needed();
        let _ = self.data.lru.push(tx_hash);

        let mut entry = self.data.by_tx_hash.entry(tx_hash).or_default();
        entry.metering = Some(metering_info);
    }

    pub fn clear_metering(&self) {
        self.data.by_tx_hash.clear();
    }

    pub fn set_metering_enabled(&self, enabled: bool) {
        self.data.metering_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.data.by_tx_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.by_tx_hash.is_empty()
    }
}

impl Default for TxDataStore {
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
        let store = TxDataStore::new(true, 100);
        let tx_hash = TxHash::random();
        let meter_data = create_test_metering(21000);

        store.insert_metering(tx_hash, meter_data);
        let data = store.get(&tx_hash);
        assert_eq!(data.metering.as_ref().unwrap().total_gas_used, 21000);

        store.insert_metering(tx_hash, create_test_metering(50000));
        let data = store.get(&tx_hash);
        assert_eq!(data.metering.as_ref().unwrap().total_gas_used, 50000);
    }

    #[test]
    fn test_clear_metering() {
        let store = TxDataStore::new(true, 100);

        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        store.insert_metering(tx1, create_test_metering(1000));
        store.insert_metering(tx2, create_test_metering(2000));

        assert!(store.get(&tx1).metering.is_some());
        assert!(store.get(&tx2).metering.is_some());

        store.clear_metering();

        assert!(store.get(&tx1).metering.is_none());
        assert!(store.get(&tx2).metering.is_none());
    }

    #[test]
    fn test_lru_eviction() {
        let store = TxDataStore::new(true, 2);

        for i in 0..3u64 {
            let tx_hash = TxHash::random();
            store.insert_metering(tx_hash, create_test_metering(i * 1000));
        }

        assert_eq!(store.len(), 2);
    }
}
