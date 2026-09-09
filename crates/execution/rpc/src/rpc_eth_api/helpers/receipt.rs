//! Loads a receipt from database. Helper trait for `eth_` block and transaction RPC methods, that
//! loads receipt data w.r.t. network.

use std::sync::Arc;

use base_common_types_chain::{TxReceipt, transaction::TransactionMeta};
use base_common_types_rpc::BaseTransactionReceipt;
use base_execution_state_api::{ProviderReceipt, ProviderTx};
use futures::Future;
use reth_primitives_traits::{Recovered, RecoveredBlock};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_rpc_eth_types::{
    BaseEthApiError, EthApiError, utils::calculate_gas_used_and_next_log_index,
};

use crate::BaseEthApi;

/// Assembles transaction receipt data w.r.t to network.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` receipts RPC methods.
impl BaseEthApi {
    /// Helper method for `eth_getBlockReceipts` and `eth_getTransactionReceipt`.
    ///
    /// If a value is `Some`, skips the corresponding cache lookup entirely.
    pub fn build_transaction_receipt(
        &self,
        tx: Recovered<ProviderTx<BlockchainProvider>>,
        meta: TransactionMeta,
        receipt: ProviderReceipt<BlockchainProvider>,
        all_receipts: Option<Arc<Vec<ProviderReceipt<BlockchainProvider>>>>,
        block: Option<Arc<RecoveredBlock>>,
    ) -> impl Future<Output = Result<BaseTransactionReceipt, BaseEthApiError>> + Send {
        async move {
            let hash = meta.block_hash;
            let (block, all_receipts) = match (block, all_receipts) {
                (Some(block), Some(all_receipts)) => (Some(block), all_receipts),
                (Some(block), None) => {
                    let all_receipts = self
                        .cache()
                        .get_receipts(hash)
                        .await
                        .map_err(BaseEthApiError::from_eth_err)?
                        .ok_or(EthApiError::HeaderNotFound(hash.into()))?;
                    (Some(block), all_receipts)
                }
                (None, Some(all_receipts)) => {
                    let block = self
                        .cache()
                        .get_maybe_block(hash)
                        .await
                        .map_err(BaseEthApiError::from_eth_err)?;
                    (block, all_receipts)
                }
                (None, None) => {
                    let (all_receipts, block) = self
                        .cache()
                        .get_receipts_and_maybe_block(hash)
                        .await
                        .map_err(BaseEthApiError::from_eth_err)?
                        .ok_or(EthApiError::HeaderNotFound(hash.into()))?;
                    (block, all_receipts)
                }
            };

            let (gas_used, next_log_index) =
                calculate_gas_used_and_next_log_index(meta.index, &all_receipts);

            let inputs = vec![ConvertReceiptInput {
                tx: tx.as_recovered_ref(),
                gas_used: receipt.cumulative_gas_used() - gas_used,
                receipt,
                next_log_index,
                meta,
            }];
            let mut receipts = match block {
                Some(block) => {
                    self.converter().convert_receipts_with_block(inputs, block.sealed_block())
                }
                None => self.converter().convert_receipts(inputs),
            }?;

            Ok(receipts.pop().unwrap())
        }
    }
}
