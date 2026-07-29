//! Shared types for in-process mempool metering keyed by transaction hash.

use std::sync::Arc;

use alloy_primitives::{Bytes, TxHash};

use crate::MeterBundleResponse;

/// Lookup and submit interface for mempool inline metering.
///
/// Used by `eth_sendRawTransaction` to kick off simulation and by the builder
/// forwarder to require a [`MeterBundleResponse`] before forwarding.
pub trait InlineMetering: std::fmt::Debug + Send + Sync + 'static {
    /// Returns a completed meterBundle response for `tx_hash`, if available.
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse>;

    /// Fires a background worker that awaits `meterBundle` and stashes the result.
    ///
    /// No-ops if a response is already cached or a worker is already in flight.
    fn submit(&self, tx_hash: TxHash, raw: Bytes);
}

/// Shared handle to an [`InlineMetering`] implementation.
pub type SharedInlineMetering = Arc<dyn InlineMetering>;
