//! Traits for the RPC module.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, TxHash};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{
    Bundle, MeterBlockResponse, MeterBundleResponse, MeteredPriorityFeeResponse,
    TransactionStatusResponse,
};

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

    /// Estimates the priority fee necessary for a bundle to be included in recently observed
    /// flashblocks, considering multiple resource constraints.
    #[method(name = "meteredPriorityFeePerGas")]
    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse>;

    /// Sets metering information for a transaction. Called by tips-ingress to push
    /// transaction resource usage data for priority fee estimation.
    #[method(name = "setMeteringInfo")]
    async fn set_metering_info(&self, tx_hash: TxHash, meter: MeterBundleResponse)
    -> RpcResult<()>;

    /// Enables or disables metering data collection.
    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    /// Clears all pending metering information.
    #[method(name = "clearMeteringInfo")]
    async fn clear_metering_info(&self) -> RpcResult<()>;
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}
