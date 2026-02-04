//! Data types for the builder storage.

use std::sync::atomic::AtomicBool;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use concurrent_queue::ConcurrentQueue;

use super::StoredBackrunBundle;

/// Transaction data stored in the builder cache.
///
/// Contains resource metering information and associated backrun bundles
/// for a given transaction hash.
#[derive(Clone, Default, Debug)]
pub struct TxData {
    /// Resource metering response from simulation.
    pub metering: Option<MeterBundleResponse>,
    /// Backrun bundles targeting this transaction, sorted by priority fee.
    pub backrun_bundles: Vec<StoredBackrunBundle>,
}

/// Internal storage data structure.
///
/// Manages transaction data with an LRU eviction policy.
pub struct StoreData {
    /// Mapping of transaction hash to transaction data.
    pub(crate) by_tx_hash: dashmap::DashMap<TxHash, TxData>,
    /// LRU queue for transaction hash eviction.
    pub(crate) lru: ConcurrentQueue<TxHash>,
    /// Whether resource metering is enabled.
    pub(crate) metering_enabled: AtomicBool,
}

impl core::fmt::Debug for StoreData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoreData")
            .field("entries", &self.by_tx_hash.len())
            .field("metering_enabled", &self.metering_enabled)
            .finish()
    }
}
