//! Helper types for `base_execution_rpc::EthApiServer` implementation.
//!
//! Types used in block building.

use std::{sync::Arc, time::Instant};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{B256, BlockHash, TxHash};
use base_common_types_chain::{BaseReceipt, BlockHeader, EthereumReceipt as Receipt};
use base_common_types_rpc::BaseTransactionReceipt;
use base_execution_evm_blocks::EvmEnvFor;
use derive_more::Constructor;
use reth_primitives_traits::{IndexedTx, RecoveredBlock, SealedHeader};
use {base_execution_state_types::ExecutedBlock, reth_chain_state::BlockState};

use crate::block::BlockAndReceipts;

/// Configured [`base_execution_evm_blocks::EvmEnv`] for a pending block.
#[derive(Debug, Clone, Constructor)]
pub struct PendingBlockEnv {
    /// Configured [`base_execution_evm_blocks::EvmEnv`] for the pending block.
    pub evm_env: EvmEnvFor,
    /// Origin block for the config
    pub origin: PendingBlockEnvOrigin<BaseReceipt>,
}

/// The origin for a configured [`PendingBlockEnv`]
#[derive(Clone, Debug)]
pub enum PendingBlockEnvOrigin<R = Receipt> {
    /// The pending block as received from the CL.
    ActualPending(Arc<RecoveredBlock>, Arc<Vec<R>>),
    /// The _modified_ header of the latest block.
    ///
    /// This derives the pending state based on the latest header by modifying:
    ///  - the timestamp
    ///  - the block number
    ///  - fees
    DerivedFromLatest(SealedHeader),
}

impl<R> PendingBlockEnvOrigin<R> {
    /// Returns true if the origin is the actual pending block as received from the CL.
    pub const fn is_actual_pending(&self) -> bool {
        matches!(self, Self::ActualPending(_, _))
    }

    /// Consumes the type and returns the actual pending block.
    pub fn into_actual_pending(self) -> Option<Arc<RecoveredBlock>> {
        match self {
            Self::ActualPending(block, _) => Some(block),
            _ => None,
        }
    }

    /// Returns the [`BlockId`] that represents the state of the block.
    ///
    /// If this is the actual pending block, the state is the "Pending" tag, otherwise we can safely
    /// identify the block by its hash (latest block).
    pub fn state_block_id(&self) -> BlockId {
        match self {
            Self::ActualPending(_, _) => BlockNumberOrTag::Pending.into(),
            Self::DerivedFromLatest(latest) => BlockId::Hash(latest.hash().into()),
        }
    }

    /// Returns the hash of the block the pending block should be built on.
    ///
    /// For the [`PendingBlockEnvOrigin::ActualPending`] this is the parent hash of the block.
    /// For the [`PendingBlockEnvOrigin::DerivedFromLatest`] this is the hash of the _latest_
    /// header.
    pub fn build_target_hash(&self) -> B256 {
        match self {
            Self::ActualPending(block, _) => block.header().parent_hash(),
            Self::DerivedFromLatest(latest) => latest.hash(),
        }
    }
}

/// A type alias for a pair of an [`Arc`] wrapped [`RecoveredBlock`] and a vector of
/// [`base_common_types_chain::BaseReceipt`].
pub type PendingBlockAndReceipts = BlockAndReceipts;

/// Locally built pending block for `pending` tag.
#[derive(Debug, Clone, Constructor)]
pub struct PendingBlock {
    /// Timestamp when the pending block is considered outdated.
    pub expires_at: Instant,
    /// The receipts for the pending block
    pub receipts: Arc<Vec<BaseReceipt>>,
    /// The locally built pending block with execution output.
    pub executed_block: ExecutedBlock,
}

impl PendingBlock {
    /// Creates a new instance of [`PendingBlock`] with `executed_block` as its output that should
    /// not be used past `expires_at`.
    pub fn with_executed_block(expires_at: Instant, executed_block: ExecutedBlock) -> Self {
        Self {
            expires_at,
            receipts: Arc::new(executed_block.execution_output.receipts.clone()),
            executed_block,
        }
    }

    /// Returns the locally built pending [`RecoveredBlock`].
    pub const fn block(&self) -> &Arc<RecoveredBlock> {
        &self.executed_block.recovered_block
    }

    /// Converts this [`PendingBlock`] into a pair of [`RecoveredBlock`] and a vector of
    /// [`base_common_types_chain::BaseReceipt`]s, taking self.
    pub fn into_block_and_receipts(self) -> PendingBlockAndReceipts {
        BlockAndReceipts { block: self.executed_block.recovered_block, receipts: self.receipts }
    }

    /// Returns a pair of [`RecoveredBlock`] and a vector of  [`base_common_types_chain::BaseReceipt`]s by
    /// cloning from borrowed self.
    pub fn to_block_and_receipts(&self) -> PendingBlockAndReceipts {
        BlockAndReceipts {
            block: self.executed_block.recovered_block.clone(),
            receipts: self.receipts.clone(),
        }
    }

    /// Returns a hash of the parent block for this `executed_block`.
    pub fn parent_hash(&self) -> BlockHash {
        self.executed_block.recovered_block().parent_hash()
    }

    /// Finds a transaction by hash and returns it along with its corresponding receipt.
    ///
    /// Returns `None` if the transaction is not found in this block.
    pub fn find_transaction_and_receipt_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> Option<(IndexedTx<'_>, &BaseReceipt)> {
        let indexed_tx = self.executed_block.recovered_block().find_indexed(tx_hash)?;
        let receipt = self.receipts.get(indexed_tx.index())?;
        Some((indexed_tx, receipt))
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
        C: base_execution_state_api::BlockReader<
                Block = base_common_types_chain::BaseBlock,
                Transaction = base_common_types_chain::BaseTxEnvelope,
            > + base_common_chain_config::ChainSpecProvider
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
    {
        self.to_block_and_receipts().find_and_convert_transaction_receipt(tx_hash, converter)
    }
}

impl From<PendingBlock> for BlockState {
    fn from(pending_block: PendingBlock) -> Self {
        Self::new(pending_block.executed_block)
    }
}
