//! Base transaction metadata used by RPC responses.

use std::fmt::{Debug, Formatter};

use alloy_rpc_types_eth::TransactionInfo;
use base_common_consensus::{BaseReceipt, BaseTransaction, BaseTransactionInfo, DepositInfo};
use reth_primitives_traits::SignedTransaction;
use reth_rpc_convert::TxInfoMapper;
use reth_storage_api::{ReceiptProvider, TransactionsProvider, errors::ProviderError};

use crate::BaseTimeCache;

/// Base implementation of [`TxInfoMapper`].
///
/// For deposits, receipt is fetched to extract `deposit_nonce` and `deposit_receipt_version`.
/// Otherwise, it works like regular Ethereum implementation, i.e. uses [`TransactionInfo`].
pub struct BaseTxInfoMapper<Provider> {
    provider: Provider,
    base_time: BaseTimeCache,
}

impl<Provider: Clone> Clone for BaseTxInfoMapper<Provider> {
    fn clone(&self) -> Self {
        Self { provider: self.provider.clone(), base_time: self.base_time.clone() }
    }
}

impl<Provider> Debug for BaseTxInfoMapper<Provider> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseTxInfoMapper").finish()
    }
}

impl<Provider> BaseTxInfoMapper<Provider> {
    /// Creates a mapper backed by the given provider and `BaseTime` cache.
    pub const fn new(provider: Provider, base_time: BaseTimeCache) -> Self {
        Self { provider, base_time }
    }
}

impl<T, Provider> TxInfoMapper<T> for BaseTxInfoMapper<Provider>
where
    T: BaseTransaction + SignedTransaction,
    Provider: TransactionsProvider<Transaction = T> + ReceiptProvider<Receipt = BaseReceipt>,
{
    type Out = BaseTransactionInfo;
    type Err = ProviderError;

    fn try_map(&self, tx: &T, tx_info: TransactionInfo) -> Result<Self::Out, ProviderError> {
        let deposit_meta = if tx.is_deposit() {
            self.provider.receipt_by_hash(*tx.tx_hash())?.and_then(|receipt| {
                receipt.as_deposit_receipt().map(|receipt| DepositInfo {
                    deposit_receipt_version: receipt.deposit_receipt_version,
                    deposit_nonce: receipt.deposit_nonce,
                })
            })
        } else {
            None
        }
        .unwrap_or_default();

        let block_timestamp_ms =
            match (tx_info.block_hash, tx_info.block_number, tx_info.block_timestamp) {
                (Some(block_hash), Some(block_number), Some(block_timestamp)) => self
                    .base_time
                    .get::<T, _>(&self.provider, block_hash, block_number, block_timestamp)?,
                _ => None,
            };

        Ok(BaseTransactionInfo { inner: tx_info, deposit_meta, block_timestamp_ms })
    }
}
