//! Sink for inserting metering responses into the builder's `MeteringStore`.

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;

/// Inserts a [`MeterBundleResponse`] into the builder metering store.
///
/// Defined in this crate so [`BuilderApiImpl`] can accept a type-erased sink
/// without depending on `base-builder-core`.
pub trait MeteringResponseSink: std::fmt::Debug + Send + Sync + 'static {
    /// Stores metering information for `tx_hash`.
    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse);
}

/// Shared type-erased metering response sink.
pub type SharedMeteringResponseSink = std::sync::Arc<dyn MeteringResponseSink>;
