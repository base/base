//! Database access for `eth_` block RPC methods. Loads block and receipt data w.r.t. network.

use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{Block, BlockTransactions, Index};
use base_common_consensus::{TxReceipt, transaction::TxHashRef};
use base_common_rpc_types::{BaseBlockResponse, BaseTransactionReceipt};
use base_execution_txpool::{PoolTransaction, TransactionPool};
use futures::Future;
use reth_primitives_traits::{
    AlloyBlockHeader, BlockBody, RecoveredBlock, SealedHeader, TransactionMeta,
};
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_rpc_eth_types::BaseEthApiError;
use reth_storage_api::{BlockIdReader, BlockReader, ProviderHeader, ProviderTx};

use crate::{BaseEthApi, FromEthApiError, RpcNodeCore, RpcNodeCoreExt};

/// Result type of the fetched block receipts.
pub type BlockReceiptsResult<E> = Result<Option<Vec<BaseTransactionReceipt>>, E>;
/// Result type of the fetched block and its receipts.
pub type BlockAndReceiptsResult = Result<
    Option<(Arc<RecoveredBlock>, Arc<Vec<base_common_consensus::BaseReceipt>>)>,
    BaseEthApiError,
>;

/// Block related functions for the [`EthApiServer`](crate::EthApiServer) trait in the
/// `eth_` namespace.
impl<N: RpcNodeCore> BaseEthApi<N> {
    /// Returns the number transactions in the given block.
    ///
    /// Returns `None` if the block does not exist
    pub fn block_transaction_count(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<usize>, BaseEthApiError>> + Send {
        async move { Ok(self.recovered_block(block_id).await?.map(|b| b.body().transaction_count())) }
    }

    /// Helper function for `eth_getBlockReceipts`.
    ///
    /// Returns all transaction receipts in block, or `None` if block wasn't found.
    pub fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockReceiptsResult<BaseEthApiError>> + Send {
        async move {
            if let Some((block, receipts)) = self.load_block_and_receipts(block_id).await? {
                let block_number = block.number();
                let base_fee = block.base_fee_per_gas();
                let block_hash = block.hash();
                let excess_blob_gas = block.excess_blob_gas();
                let timestamp = block.timestamp();
                let mut gas_used = 0;
                let mut next_log_index = 0;

                let inputs = block
                    .transactions_recovered()
                    .zip(Arc::unwrap_or_clone(receipts))
                    .enumerate()
                    .map(|(idx, (tx, receipt))| {
                        let meta = TransactionMeta {
                            tx_hash: *tx.tx_hash(),
                            index: idx as u64,
                            block_hash,
                            block_number,
                            base_fee,
                            excess_blob_gas,
                            timestamp,
                        };

                        let cumulative_gas_used = receipt.cumulative_gas_used();
                        let logs_len = receipt.logs().len();

                        let input = ConvertReceiptInput {
                            tx,
                            gas_used: cumulative_gas_used - gas_used,
                            next_log_index,
                            meta,
                            receipt,
                        };

                        gas_used = cumulative_gas_used;
                        next_log_index += logs_len;

                        input
                    })
                    .collect::<Vec<_>>();

                return Ok(self
                    .converter()
                    .convert_receipts_with_block(inputs, block.sealed_block())
                    .map(Some)?);
            }

            Ok(None)
        }
    }

    /// Helper method that loads a block and all its receipts.
    pub fn load_block_and_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockAndReceiptsResult> + Send
    where
        N::Pool: TransactionPool<Transaction: PoolTransaction<Consensus = ProviderTx<N::Provider>>>,
    {
        async move {
            if block_id.is_pending() {
                if self.pending_block_kind().is_none() {
                    return Ok(None);
                }

                // First, try to get the pending block from the provider, in case we already
                // received the actual pending block from the CL.
                if let Some((block, receipts)) = self
                    .provider()
                    .pending_block_and_receipts()
                    .map_err(BaseEthApiError::from_eth_err)?
                {
                    return Ok(Some((Arc::new(block), Arc::new(receipts))));
                }

                // If no pending block from provider, build the pending block locally.
                if let Some(pending) = self.local_pending_block().await? {
                    return Ok(Some((pending.block, pending.receipts)));
                }
            }

            if let Some(block_hash) = self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(BaseEthApiError::from_eth_err)?
                && let Some((block, receipts)) = self
                    .cache()
                    .get_block_and_receipts(block_hash)
                    .await
                    .map_err(BaseEthApiError::from_eth_err)?
            {
                return Ok(Some((block, receipts)));
            }

            Ok(None)
        }
    }

    /// Returns uncle headers of given block.
    ///
    /// Returns an empty vec if there are none.
    #[expect(clippy::type_complexity)]
    pub fn ommers(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Vec<ProviderHeader>>, BaseEthApiError>> + Send {
        async move {
            if let Some(block) = self.recovered_block(block_id).await? {
                Ok(block.body().ommers().map(|o| o.to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// Returns uncle block at given index in given block.
    ///
    /// Returns `None` if index out of range.
    pub fn ommer_by_block_and_index(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> impl Future<Output = Result<Option<BaseBlockResponse>, BaseEthApiError>> + Send {
        async move {
            let uncles = self
                .recovered_block(block_id)
                .await?
                .map(|block| block.body().ommers().map(|o| o.to_vec()).unwrap_or_default())
                .unwrap_or_default();

            uncles
                .into_iter()
                .nth(index.into())
                .map(|header| {
                    let block =
                        base_common_consensus::Block::<base_common_consensus::TxEnvelope, _>::uncle(
                            header,
                        );
                    let size = block.length();
                    let header = self
                        .converter()
                        .convert_header(SealedHeader::new_unhashed(block.header), size)?;
                    Ok(Block {
                        uncles: vec![],
                        header,
                        transactions: BlockTransactions::Uncle,
                        withdrawals: None,
                    })
                })
                .transpose()
        }
    }
}

/// Loads a block from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` blocks RPC methods.
impl<N: RpcNodeCore> BaseEthApi<N> {
    /// Returns the block object for the given block id.
    #[expect(clippy::type_complexity)]
    pub fn recovered_block(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Arc<RecoveredBlock>>, BaseEthApiError>> + Send {
        async move {
            if block_id.is_pending() {
                if self.pending_block_kind().is_none() {
                    return Ok(None);
                }

                // Pending block can be fetched directly without need for caching
                if let Some(pending_block) =
                    self.provider().pending_block().map_err(BaseEthApiError::from_eth_err)?
                {
                    return Ok(Some(Arc::new(pending_block)));
                }

                // If no pending block from provider, try to get local pending block
                return match self.local_pending_block().await? {
                    Some(pending) => Ok(Some(pending.block)),
                    None => Ok(None),
                };
            }

            let block_hash = match self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(BaseEthApiError::from_eth_err)?
            {
                Some(block_hash) => block_hash,
                None => return Ok(None),
            };

            self.cache()
                .get_recovered_block(block_hash)
                .await
                .map_err(BaseEthApiError::from_eth_err)
        }
    }
}
