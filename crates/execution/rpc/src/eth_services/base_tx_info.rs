//! Base transaction metadata used by RPC responses.

use base_common_types_chain::{BaseTransactionInfo, BaseTxEnvelope, DepositInfo};
use base_common_types_rpc::TransactionInfo;
use base_execution_state_types::{ProviderError, ReceiptProvider};

impl crate::BaseRpcConverter {
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
