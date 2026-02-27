//! Traits for the metering RPC module.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_bundles::{Bundle, MeterBundleResponse};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{MeterBlockResponse, MeteredPriorityFeeResponse};

/// RPC API for transaction metering
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;

    /// Handler for: `base_meterBlockByHash`
    ///
    /// Re-executes a block and returns timing metrics for EVM execution and state root calculation.
    ///
    /// This method fetches the block by hash, re-executes all transactions against the parent
    /// block's state, and measures:
    /// - `executionTimeUs`: Time to execute all transactions in the EVM
    /// - `stateRootTimeUs`: Time to compute the state root after execution
    /// - `totalTimeUs`: Sum of execution and state root calculation time
    /// - `meteredTransactions`: Per-transaction execution times and gas usage
    #[method(name = "meterBlockByHash")]
    async fn meter_block_by_hash(&self, hash: B256) -> RpcResult<MeterBlockResponse>;

    /// Handler for: `base_meterBlockByNumber`
    ///
    /// Re-executes a block and returns timing metrics for EVM execution and state root calculation.
    ///
    /// This method fetches the block by number, re-executes all transactions against the parent
    /// block's state, and measures:
    /// - `executionTimeUs`: Time to execute all transactions in the EVM
    /// - `stateRootTimeUs`: Time to compute the state root after execution
    /// - `totalTimeUs`: Sum of execution and state root calculation time
    /// - `meteredTransactions`: Per-transaction execution times and gas usage
    #[method(name = "meterBlockByNumber")]
    async fn meter_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<MeterBlockResponse>;

    /// Handler for: `base_meteredPriorityFeePerGas`
    ///
    /// Simulates a bundle, meters its resource consumption, and returns a recommended priority
    /// fee based on recent block congestion.
    ///
    /// The algorithm:
    /// 1. Meters the bundle (same as `meterBundle`)
    /// 2. Computes resource demand from the metering results
    /// 3. Uses recent block data to estimate the minimum priority fee that would have
    ///    achieved inclusion for each resource type
    /// 4. Returns the maximum fee across all resources as the recommended priority fee
    #[method(name = "meteredPriorityFeePerGas")]
    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse>;
}
