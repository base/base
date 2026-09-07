//! Compatibility functions for rpc `Transaction` type.
use std::fmt::Debug;

use alloy_consensus::transaction::Recovered;
use alloy_rpc_types_eth::Log;
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_primitives_traits::{SealedBlock, TransactionMeta};

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

/// A type that knows how to convert primitive receipts to RPC representations.
pub trait ReceiptConverter: Debug + 'static {
    /// RPC receipt representation.
    type RpcReceipt;

    /// RPC log representation.
    type RpcLog;

    /// Error that may occur during conversion.
    type Error;

    /// Converts an RPC log using its primitive receipt and block header.
    fn convert_log(
        &self,
        log: Log,
        receipt: &BaseReceipt,
        header: &reth_primitives_traits::SealedHeader,
    ) -> Result<Self::RpcLog, Self::Error>;

    /// Converts a set of primitive receipts to RPC representations. It is guaranteed that all
    /// receipts are from the same block.
    fn convert_receipts(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error>;

    /// Converts primitive receipts from `block` to RPC representations.
    fn convert_receipts_with_block(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
        _block: &SealedBlock<BaseBlock>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        self.convert_receipts(receipts)
    }
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
