//! Compatibility functions for rpc `Transaction` type.
use core::error;
use std::fmt::Debug;

use alloy_consensus::transaction::Recovered;
use alloy_rpc_types_eth::Log;
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_evm::{BlockEnvFor, EvmEnvFor, SpecFor, TxEnvFor};
use reth_primitives_traits::{SealedBlock, TransactionMeta};

use crate::TryIntoTxEnv;

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

/// Converts `TxReq` into `TxEnv`.
///
/// Where:
/// * `TxReq` is a transaction request received from an RPC API
/// * `TxEnv` is the corresponding transaction environment for execution
///
/// The `TxEnvConverter` has two blanket implementations:
/// * `()` assuming `TxReq` implements [`TryIntoTxEnv`] and is used as default for `BaseRpcConverter`.
/// * `Fn(TxReq, &CfgEnv<Spec>, &BlockEnv) -> Result<TxEnv, E>` and can be applied using
///   a custom transaction environment conversion function.
///
/// One should prefer to implement [`TryIntoTxEnv`] for `TxReq` to get the `TxEnvConverter`
/// implementation for free, thanks to the blanket implementation, unless the conversion requires
/// more context. For example, some configuration parameters or access handles to database, network,
/// etc.
pub trait TxEnvConverter<TxReq>: Debug + Send + Sync + Unpin + Clone + 'static {
    /// An associated error that can occur during conversion.
    type Error;

    /// Converts a rpc transaction request into a transaction environment.
    ///
    /// See [`TxEnvConverter`] for more information.
    fn convert_tx_env(&self, tx_req: TxReq, evm_env: &EvmEnvFor) -> Result<TxEnvFor, Self::Error>;
}

impl<TxReq> TxEnvConverter<TxReq> for ()
where
    TxReq: TryIntoTxEnv<TxEnvFor, SpecFor, BlockEnvFor>,
{
    type Error = TxReq::Err;

    fn convert_tx_env(&self, tx_req: TxReq, evm_env: &EvmEnvFor) -> Result<TxEnvFor, Self::Error> {
        tx_req.try_into_tx_env(evm_env)
    }
}

/// Converts rpc transaction requests into transaction environment using a closure.
impl<F, TxReq, E> TxEnvConverter<TxReq> for F
where
    F: Fn(TxReq, &EvmEnvFor) -> Result<TxEnvFor, E> + Debug + Send + Sync + Unpin + Clone + 'static,
    TxReq: Clone,
    E: error::Error + Send + Sync + 'static,
{
    type Error = E;

    fn convert_tx_env(&self, tx_req: TxReq, evm_env: &EvmEnvFor) -> Result<TxEnvFor, Self::Error> {
        self(tx_req, evm_env)
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
