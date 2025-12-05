//! Traits for the transaction status module.

use alloy_primitives::TxHash;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::TransactionStatusResponse;

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}
