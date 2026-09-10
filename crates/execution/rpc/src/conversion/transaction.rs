//! Compatibility functions for rpc `Transaction` type.
use std::fmt::Debug;

use base_common_types_chain::{
    BaseReceipt, BaseTxEnvelope, TransactionMeta, transaction::Recovered,
};

/// Primitive receipt and transaction context used to construct a Base RPC receipt.
#[derive(Debug, Clone)]
pub struct ConvertReceiptInput<'a> {
    /// Primitive receipt.
    pub receipt: BaseReceipt,
    /// Transaction the receipt corresponds to.
    pub tx: Recovered<&'a BaseTxEnvelope>,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Number of logs emitted before this transaction.
    pub next_log_index: usize,
    /// Metadata for the transaction.
    pub meta: TransactionMeta,
}

/// Conversion into transaction RPC response failed.
#[derive(Debug, thiserror::Error)]
pub enum TransactionConversionError {
    /// Required fields are missing from the transaction request.
    #[error("Failed to convert transaction into RPC response: {0}")]
    FromTxReq(String),

    /// Other conversion errors.
    #[error("{0}")]
    Other(String),
}
