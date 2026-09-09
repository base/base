//! Base transaction metadata used by RPC responses.

use std::fmt::{Debug, Formatter};

use base_common_types_chain::{BaseTransactionInfo, BaseTxEnvelope, DepositInfo};
use base_common_types_rpc::TransactionInfo;
use base_execution_state_api::{ProviderError, ReceiptProvider, TransactionsProvider};

use crate::BaseTimeCache;

/// Enriches transaction metadata with Base deposit and timestamp fields.
///
/// For deposits, receipt is fetched to extract `deposit_nonce` and `deposit_receipt_version`.
/// Otherwise, it works like regular Ethereum implementation, i.e. uses [`TransactionInfo`].
pub struct BaseTxInfoMapper<Provider> {
    provider: Provider,
    pub base_time: BaseTimeCache,
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

impl<Provider> BaseTxInfoMapper<Provider>
where
    Provider: TransactionsProvider<Transaction = BaseTxEnvelope> + ReceiptProvider,
{
    /// Loads deposit receipt fields and the block timestamp for a transaction.
    pub fn try_map(
        &self,
        tx: &BaseTxEnvelope,
        tx_info: TransactionInfo,
    ) -> Result<BaseTransactionInfo, ProviderError> {
        let deposit_meta = if tx.is_deposit() {
            self.provider.receipt_by_hash(tx.tx_hash())?.and_then(|receipt| {
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
                (Some(block_hash), Some(block_number), Some(block_timestamp)) => {
                    self.base_time.get::<BaseTxEnvelope, _>(
                        &self.provider,
                        block_hash,
                        block_number,
                        block_timestamp,
                    )?
                }
                _ => None,
            };

        Ok(BaseTransactionInfo { inner: tx_info, deposit_meta, block_timestamp_ms })
    }
}
