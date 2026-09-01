//! Traits for the metering RPC module.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_bundles::{Bundle, MeterBundleResponse};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::MeterBlockResponse;

/// RPC API for transaction metering.
///
/// The API exposes bundle simulation and block profiling.
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions against latest canonical state.
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;

    /// Handler for: `base_meterBlockByHash`
    ///
    /// Re-executes a block and returns timing metrics for signer recovery and EVM execution.
    ///
    /// This method fetches the block by hash, re-executes all transactions against the parent
    /// block's state, and measures:
    /// - `executionTimeUs`: Time to execute all transactions in the EVM
    /// - `totalTimeUs`: Sum of signer recovery and execution time
    /// - `meteredTransactions`: Per-transaction execution times and gas usage
    #[method(name = "meterBlockByHash")]
    async fn meter_block_by_hash(&self, hash: B256) -> RpcResult<MeterBlockResponse>;

    /// Handler for: `base_meterBlockByNumber`
    ///
    /// Re-executes a block and returns timing metrics for signer recovery and EVM execution.
    ///
    /// This method fetches the block by number, re-executes all transactions against the parent
    /// block's state, and measures:
    /// - `executionTimeUs`: Time to execute all transactions in the EVM
    /// - `totalTimeUs`: Sum of signer recovery and execution time
    /// - `meteredTransactions`: Per-transaction execution times and gas usage
    #[method(name = "meterBlockByNumber")]
    async fn meter_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<MeterBlockResponse>;
}
