//! Block related types for RPC API.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, TxReceipt, transaction::TransactionMeta};
use alloy_primitives::TxHash;
use base_common_consensus::{BaseBlock, BaseReceipt};
use base_common_rpc_types::BaseTransactionReceipt;
use reth_primitives_traits::{Block, BlockBody, IndexedTx, Recovered, RecoveredBlock, SealedBlock};
use reth_rpc_convert::transaction::ConvertReceiptInput;

use crate::{TransactionSource, utils::calculate_gas_used_and_next_log_index};

/// Cached data for a transaction lookup.
#[derive(Debug, Clone)]
pub struct CachedTransaction<B: Block, R> {
    /// The block containing this transaction.
    pub block: Arc<RecoveredBlock<B>>,
    /// Index of the transaction within the block.
    pub tx_index: usize,
    /// Receipts for the block, if available.
    pub receipts: Option<Arc<Vec<R>>>,
}

impl<B: Block, R> CachedTransaction<B, R> {
    /// Creates a new cached transaction entry.
    pub const fn new(
        block: Arc<RecoveredBlock<B>>,
        tx_index: usize,
        receipts: Option<Arc<Vec<R>>>,
    ) -> Self {
        Self { block, tx_index, receipts }
    }

    /// Returns the `Recovered<&T>` transaction at the cached index.
    pub fn recovered_transaction(&self) -> Option<Recovered<&<B::Body as BlockBody>::Transaction>> {
        self.block.recovered_transaction(self.tx_index)
    }

    /// Converts this cached transaction into a [`TransactionSource::Block`].
    ///
    /// Returns `None` if the transaction index is out of bounds.
    pub fn to_transaction_source(
        &self,
    ) -> Option<TransactionSource<<B::Body as BlockBody>::Transaction>> {
        let tx = self.recovered_transaction()?;
        Some(TransactionSource::Block {
            transaction: tx.cloned(),
            index: self.tx_index as u64,
            block_hash: self.block.hash(),
            block_number: self.block.number(),
            block_timestamp: self.block.timestamp(),
            base_fee: self.block.base_fee_per_gas(),
        })
    }

    /// Returns the receipt at the cached transaction index, if receipts are available.
    pub fn receipt(&self) -> Option<&R> {
        self.receipts.as_ref()?.get(self.tx_index)
    }

    /// Constructs a [`TransactionMeta`] for this cached transaction using the given tx hash.
    pub fn transaction_meta(&self, tx_hash: TxHash) -> TransactionMeta
    where
        B::Header: BlockHeader,
    {
        TransactionMeta {
            tx_hash,
            index: self.tx_index as u64,
            block_hash: self.block.hash(),
            block_number: self.block.number(),
            base_fee: self.block.base_fee_per_gas(),
            excess_blob_gas: self.block.header().excess_blob_gas(),
            timestamp: self.block.timestamp(),
        }
    }
}

/// A pair of an [`Arc`] wrapped [`RecoveredBlock`] and its corresponding receipts.
///
/// This type is used throughout the RPC layer to efficiently pass around
/// blocks with their execution receipts, avoiding unnecessary cloning.
#[derive(Debug, Clone)]
pub struct BlockAndReceipts {
    /// The recovered block.
    pub block: Arc<RecoveredBlock<BaseBlock>>,
    /// The receipts for the block.
    pub receipts: Arc<Vec<BaseReceipt>>,
}

impl BlockAndReceipts {
    /// Creates a new [`BlockAndReceipts`] instance.
    pub const fn new(
        block: Arc<RecoveredBlock<BaseBlock>>,
        receipts: Arc<Vec<BaseReceipt>>,
    ) -> Self {
        Self { block, receipts }
    }

    /// Finds a transaction by hash and returns it along with its corresponding receipt.
    ///
    /// Returns `None` if the transaction is not found in this block.
    pub fn find_transaction_and_receipt_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> Option<(IndexedTx<'_, BaseBlock>, &BaseReceipt)> {
        let indexed_tx = self.block.find_indexed(tx_hash)?;
        let receipt = self.receipts.get(indexed_tx.index())?;
        Some((indexed_tx, receipt))
    }

    /// Returns the underlying sealed block.
    pub fn sealed_block(&self) -> &SealedBlock<BaseBlock> {
        self.block.sealed_block()
    }

    /// Returns the rpc transaction receipt for the given transaction hash if it exists.
    ///
    /// This uses the given converter to turn [`Self::find_transaction_and_receipt_by_hash`] into
    /// the rpc format.
    pub fn find_and_convert_transaction_receipt<C>(
        &self,
        tx_hash: TxHash,
        converter: &crate::BaseRpcConverter<C>,
    ) -> Option<Result<BaseTransactionReceipt, crate::BaseEthApiError>>
    where
        C: reth_storage_api::BlockReader<
                Block = base_common_consensus::BaseBlock,
                Transaction = base_common_consensus::BaseTxEnvelope,
                Receipt = base_common_consensus::BaseReceipt,
            > + base_execution_chainspec::ChainSpecProvider
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
    {
        let (tx, receipt) = self.find_transaction_and_receipt_by_hash(tx_hash)?;
        convert_transaction_receipt(
            self.block.as_ref(),
            self.receipts.as_ref(),
            tx,
            receipt,
            converter,
        )
    }
}

/// Converts a transaction and its receipt into the rpc receipt format using the given converter.
pub fn convert_transaction_receipt<C>(
    block: &RecoveredBlock<BaseBlock>,
    all_receipts: &[BaseReceipt],
    tx: IndexedTx<'_, BaseBlock>,
    receipt: &BaseReceipt,
    converter: &crate::BaseRpcConverter<C>,
) -> Option<Result<BaseTransactionReceipt, crate::BaseEthApiError>>
where
    C: reth_storage_api::BlockReader<
            Block = base_common_consensus::BaseBlock,
            Transaction = base_common_consensus::BaseTxEnvelope,
            Receipt = base_common_consensus::BaseReceipt,
        > + base_execution_chainspec::ChainSpecProvider
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
{
    let meta = tx.meta();
    let (gas_used, next_log_index) =
        calculate_gas_used_and_next_log_index(meta.index, all_receipts);

    converter
        .convert_receipts_with_block(
            vec![ConvertReceiptInput {
                tx: tx.recovered_tx(),
                gas_used: receipt.cumulative_gas_used() - gas_used,
                receipt: receipt.clone(),
                next_log_index,
                meta,
            }],
            block.sealed_block(),
        )
        .map(|mut receipts| receipts.pop())
        .transpose()
}

impl CachedTransaction<BaseBlock, BaseReceipt> {
    /// Converts this cached transaction into an RPC receipt using the given converter.
    ///
    /// Returns `None` if receipts are not available or the transaction index is out of bounds.
    pub fn into_receipt<C>(
        self,
        converter: &crate::BaseRpcConverter<C>,
    ) -> Option<Result<BaseTransactionReceipt, crate::BaseEthApiError>>
    where
        C: reth_storage_api::BlockReader<
                Block = base_common_consensus::BaseBlock,
                Transaction = base_common_consensus::BaseTxEnvelope,
                Receipt = base_common_consensus::BaseReceipt,
            > + base_execution_chainspec::ChainSpecProvider
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
    {
        let receipts = self.receipts?;
        let receipt = receipts.get(self.tx_index)?;
        let tx_hash = self.block.body().transactions.get(self.tx_index)?.tx_hash();
        let tx = self.block.find_indexed(tx_hash)?;
        convert_transaction_receipt::<C>(
            self.block.as_ref(),
            receipts.as_ref(),
            tx,
            receipt,
            converter,
        )
    }
}
