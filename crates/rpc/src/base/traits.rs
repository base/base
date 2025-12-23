//! Traits for the RPC module.

use alloy_primitives::TxHash;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{
    Bundle, MeterBundleResponse, TransactionStatusResponse,
    base::metered_fee_types::MeteredPriorityFeeResponse,
};

/// RPC API for transaction metering
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;

    /// Estimates the priority fee necessary for a bundle to be included in recently observed
    /// flashblocks, considering multiple resource constraints.
    #[method(name = "meteredPriorityFeePerGas")]
    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse>;
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}
