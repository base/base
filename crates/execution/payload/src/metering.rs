//! Lookup store for `meterBundle` results used by native payload admission.

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use parking_lot::RwLock;

/// Trait abstracting resource metering data retrieval for the native payload builder.
pub trait MeteringProvider: Debug + Send + Sync + 'static {
    /// Retrieves the metering data for a given transaction hash.
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse>;

    /// Returns whether resource metering lookups are currently enabled.
    fn is_enabled(&self) -> bool {
        false
    }

    /// Inserts metering information for a transaction.
    fn insert(&self, _tx_hash: TxHash, _metering: MeterBundleResponse) {}

    /// Removes metering data for the given transaction hashes.
    fn remove(&self, _tx_hashes: &[TxHash]) {}

    /// Clears all stored metering data.
    fn clear(&self) {}

    /// Enables or disables resource metering lookups.
    fn set_enabled(&self, _enabled: bool) {}
}

/// A no-op provider that always returns no metering data.
#[derive(Debug, Clone, Default)]
pub struct NoopMeteringProvider;

impl MeteringProvider for NoopMeteringProvider {
    fn get(&self, _tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        None
    }
}

/// In-memory `meterBundle` result store shared by the native builder and RPC.
#[derive(Debug)]
pub struct MemoryMeteringStore {
    entries: RwLock<HashMap<TxHash, MeterBundleResponse>>,
    enabled: AtomicBool,
}

impl MemoryMeteringStore {
    /// Creates a store with the given enabled flag.
    pub fn new(enabled: bool) -> Self {
        Self { entries: RwLock::new(HashMap::new()), enabled: AtomicBool::new(enabled) }
    }
}

impl Default for MemoryMeteringStore {
    fn default() -> Self {
        Self::new(false)
    }
}

impl MeteringProvider for MemoryMeteringStore {
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        self.entries.read().get(tx_hash).cloned()
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        self.entries.write().insert(tx_hash, metering);
    }

    fn remove(&self, tx_hashes: &[TxHash]) {
        let mut entries = self.entries.write();
        for tx_hash in tx_hashes {
            entries.remove(tx_hash);
        }
    }

    fn clear(&self) {
        self.entries.write().clear();
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
}

/// Type alias for the shared, type-erased metering provider.
pub type SharedMeteringProvider = Arc<dyn MeteringProvider>;
