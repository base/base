//! Trait abstraction for resource metering providers.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use base_bundles::MeterBundleResponse;

/// Trait abstracting resource metering data retrieval and management for the builder.
///
/// Entries are keyed by the hash of the exact EIP-2718 encoding they were metered for
/// ([`Encodable2718::trie_hash`]). That is the transaction hash for every type except EIP-8130,
/// whose encodings that differ only in authorization share a transaction hash but can use
/// different gas and execution time.
///
/// [`Encodable2718::trie_hash`]: alloy_eips::eip2718::Encodable2718::trie_hash
pub trait MeteringProvider: Debug + Send + Sync + 'static {
    /// Retrieves the metering data for the encoding with hash `encoded_hash`.
    ///
    /// The response is shared rather than copied because lookups run once per
    /// candidate transaction and most callers read only a few scalar fields.
    fn get(&self, encoded_hash: &B256) -> Option<Arc<MeterBundleResponse>>;

    /// Returns whether resource metering is currently enabled.
    fn is_enabled(&self) -> bool {
        false
    }

    /// Inserts metering information for the encoding with hash `encoded_hash`.
    fn insert(&self, _encoded_hash: B256, _metering: MeterBundleResponse) {}

    /// Signals that a transaction was committed without metering data.
    ///
    /// Implementations can use this to track metering data that arrives after
    /// payload inclusion.
    fn mark_included_without_metering(&self, _encoded_hash: &B256) {}

    /// Signals that a transaction was skipped (e.g. `MeteringDataPending`) and
    /// will be retried later, clearing any pending late-arrival tracking.
    fn skip(&self, _encoded_hash: &B256) {}

    /// Removes metering data for the given encoding hashes.
    ///
    /// Used to eagerly evict entries for transactions that have been included in
    /// a payload or permanently rejected so they don't occupy LRU slots that
    /// should go to pending transactions.
    fn remove(&self, _encoded_hashes: &[B256]) {}

    /// Clears all stored metering data.
    fn clear(&self) {}

    /// Enables or disables resource metering.
    fn set_enabled(&self, _enabled: bool) {}
}

/// A no-op provider that always returns no metering data.
#[derive(Debug, Clone, Default)]
pub struct NoopMeteringProvider;

impl MeteringProvider for NoopMeteringProvider {
    fn get(&self, _encoded_hash: &B256) -> Option<Arc<MeterBundleResponse>> {
        None
    }
}

/// Type alias for the shared, type-erased metering provider.
pub type SharedMeteringProvider = Arc<dyn MeteringProvider>;
