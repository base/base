//! Typed `Provider` extension traits for `optimism_*` and `debug_*` JSON-RPC methods.
//!
//! These wrap the namespaced rollup-node and execution-debug methods that are not part of the
//! standard alloy `Provider` API. Using these traits replaces ad-hoc `raw_request` calls with
//! compile-time-checked method names and typed parameters/responses at every call site.

use std::future::Future;

use alloy_eips::BlockNumberOrTag;
use alloy_network::Network;
use alloy_primitives::{Bytes, U64};
use alloy_provider::Provider;
use alloy_transport::TransportResult;
use serde_json::Value;

use super::types::{OutputAtBlock, SyncStatus};

/// Extension trait exposing the `optimism_*` JSON-RPC methods served by op-node-compatible
/// rollup nodes.
///
/// Implemented for any `Provider<N>`. Methods are thin typed wrappers around the underlying
/// JSON-RPC client; retry, backoff, caching, and error mapping remain the responsibility of
/// callers.
pub trait OptimismRollupProviderExt<N: Network>: Provider<N> {
    /// Calls `optimism_outputAtBlock` for the given L2 block.
    ///
    /// `block` is serialized as the standard JSON-RPC hex-encoded block tag.
    fn optimism_output_at_block(
        &self,
        block: BlockNumberOrTag,
    ) -> impl Future<Output = TransportResult<OutputAtBlock>> + Send + '_ {
        async move { self.client().request("optimism_outputAtBlock", (block,)).await }
    }

    /// Calls `optimism_rollupConfig`.
    ///
    /// Returns the raw JSON value so callers can perform their own typed deserialization
    /// (the on-the-wire schema for `RollupConfig` varies across node implementations).
    fn optimism_rollup_config(&self) -> impl Future<Output = TransportResult<Value>> + Send + '_ {
        async move { self.client().request_noparams("optimism_rollupConfig").await }
    }

    /// Calls `optimism_syncStatus`.
    fn optimism_sync_status(
        &self,
    ) -> impl Future<Output = TransportResult<SyncStatus>> + Send + '_ {
        async move { self.client().request_noparams("optimism_syncStatus").await }
    }
}

impl<N: Network, P: Provider<N> + ?Sized> OptimismRollupProviderExt<N> for P {}

/// Extension trait exposing the `debug_*` JSON-RPC methods used by the proof clients.
///
/// Implemented for any `Provider<N>`. Methods are thin typed wrappers around the underlying
/// JSON-RPC client; retry, backoff, caching, and error mapping remain the responsibility of
/// callers.
pub trait DebugProviderExt<N: Network>: Provider<N> {
    /// Calls `debug_chainConfig`.
    ///
    /// The chain config schema varies across clients, so the raw JSON value is returned for
    /// callers to deserialize as needed.
    fn debug_chain_config(&self) -> impl Future<Output = TransportResult<Value>> + Send + '_ {
        async move { self.client().request_noparams("debug_chainConfig").await }
    }

    /// Calls `debug_getRawBlock` and returns the RLP-encoded block bytes.
    fn debug_get_raw_block(
        &self,
        block_number: u64,
    ) -> impl Future<Output = TransportResult<Bytes>> + Send + '_ {
        async move { self.client().request("debug_getRawBlock", (U64::from(block_number),)).await }
    }
}

impl<N: Network, P: Provider<N> + ?Sized> DebugProviderExt<N> for P {}
