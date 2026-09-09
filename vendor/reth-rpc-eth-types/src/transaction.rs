//! Helper types for `base_execution_rpc::EthApiServer` implementation.
//!
//! Transaction wrapper that labels transaction with its origin.

use alloy_primitives::B256;
use base_common_types_chain::{
    BaseTxEnvelope, EthereumTxEnvelope, TxEip4844, transaction::TxHashRef,
};
use base_common_types_rpc::TransactionInfo;
use reth_primitives_traits::{Recovered, SignedTransaction};

/// Represents from where a transaction was fetched.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransactionSource<T = EthereumTxEnvelope<TxEip4844>> {
    /// Transaction exists in the pool (Pending)
    Pool(Recovered<T>),
    /// Transaction already included in a block
    ///
    /// This can be a historical block or a pending block (received from the CL)
    Block {
        /// Transaction fetched via provider
        transaction: Recovered<T>,
        /// Index of the transaction in the block
        index: u64,
        /// Hash of the block.
        block_hash: B256,
        /// Number of the block.
        block_number: u64,
        /// Timestamp of the block.
        block_timestamp: u64,
        /// base fee of the block.
        base_fee: Option<u64>,
    },
}

// === impl TransactionSource ===

impl<T: SignedTransaction> TransactionSource<T> {
    /// Consumes the type and returns the wrapped transaction.
    pub fn into_recovered(self) -> Recovered<T> {
        self.into()
    }

    /// Returns the transaction and block related info, if not pending
    pub fn split(self) -> (Recovered<T>, TransactionInfo) {
        match self {
            Self::Pool(tx) => {
                let hash = *tx.tx_hash();
                (tx, TransactionInfo { hash: Some(hash), ..Default::default() })
            }
            Self::Block {
                transaction,
                index,
                block_hash,
                block_number,
                block_timestamp,
                base_fee,
            } => {
                let hash = *transaction.tx_hash();
                (
                    transaction,
                    TransactionInfo {
                        hash: Some(hash),
                        index: Some(index),
                        block_hash: Some(block_hash),
                        block_number: Some(block_number),
                        block_timestamp: Some(block_timestamp),
                        base_fee,
                    },
                )
            }
        }
    }
}

impl<T> From<TransactionSource<T>> for Recovered<T> {
    fn from(value: TransactionSource<T>) -> Self {
        match value {
            TransactionSource::Pool(tx) => tx,
            TransactionSource::Block { transaction, .. } => transaction,
        }
    }
}

impl TransactionSource<BaseTxEnvelope> {
    /// Conversion into network specific transaction type.
    pub fn into_transaction<Builder>(
        self,
        resp_builder: &crate::BaseRpcConverter<Builder>,
    ) -> Result<base_common_types_rpc::BaseTransaction, crate::BaseEthApiError>
    where
        Builder: base_execution_state_api::BlockReader<
                Block = base_common_types_chain::BaseBlock,
                Transaction = base_common_types_chain::BaseTxEnvelope,
            > + base_common_chain_config::ChainSpecProvider
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
    {
        match self {
            Self::Pool(tx) => resp_builder.fill_pending(tx),
            Self::Block {
                transaction,
                index,
                block_hash,
                block_number,
                block_timestamp,
                base_fee,
            } => {
                let tx_info = TransactionInfo {
                    hash: Some(*transaction.tx_hash()),
                    index: Some(index),
                    block_hash: Some(block_hash),
                    block_number: Some(block_number),
                    block_timestamp: Some(block_timestamp),
                    base_fee,
                };

                resp_builder.fill(transaction, tx_info)
            }
        }
    }
}
