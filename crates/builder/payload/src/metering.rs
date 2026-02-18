/// Trait abstraction for resource metering providers.
use core::fmt::Debug;
use std::sync::Arc;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;

/// Trait abstracting resource metering data retrieval and management for the builder.
pub trait MeteringProvider: Debug + Send + Sync + 'static {
    /// Retrieves the metering data for a given transaction hash.
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse>;

    /// Inserts metering information for a transaction.
    fn insert(&self, _tx_hash: TxHash, _metering: MeterBundleResponse) {}

    /// Clears all stored metering data.
    fn clear(&self) {}

    /// Enables or disables resource metering.
    fn set_enabled(&self, _enabled: bool) {}
}

/// A no-op provider that always returns no metering data.
#[derive(Debug, Clone)]
pub struct NoopMeteringProvider;

impl MeteringProvider for NoopMeteringProvider {
    fn get(&self, _tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        None
    }
}

/// Type alias for the shared, type-erased metering provider.
pub type SharedMeteringProvider = Arc<dyn MeteringProvider>;
