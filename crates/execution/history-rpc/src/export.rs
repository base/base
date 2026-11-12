//! `archive_` export endpoint served from the node's own provider (TDD §3).

use alloy_consensus::Header;
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_history_store::{
    ArchiveApiServer, ArchiveBlock, ArchiveRpcError, ArchiveStatus, BlockData, BlockRows, BodyIndices,
    RangeBounds,
};
use jsonrpsee::core::{RpcResult, async_trait};
use reth_prune_types::PruneSegment;
use reth_storage_api::{
    BlockBodyIndicesProvider, BlockIdReader, BlockNumReader, ChangeSetReader, HeaderProvider,
    PruneCheckpointReader, ReceiptProvider, StorageChangeSetReader, TransactionsProvider,
};

/// Range caps of the export endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveExportLimits {
    /// Maximum blocks per `archive_getBlockRange` call.
    pub max_range_blocks: u64,
    /// Maximum payload bytes per `archive_getBlockRange` call.
    pub max_range_bytes: u64,
}

/// Provider capabilities the export endpoint needs.
pub trait ArchiveExportProvider:
    BlockNumReader
    + BlockIdReader
    + HeaderProvider<Header = Header>
    + BlockBodyIndicesProvider
    + TransactionsProvider<Transaction = BaseTxEnvelope>
    + ReceiptProvider<Receipt = BaseReceipt>
    + ChangeSetReader
    + StorageChangeSetReader
    + PruneCheckpointReader
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<P> ArchiveExportProvider for P where
    P: BlockNumReader
        + BlockIdReader
        + HeaderProvider<Header = Header>
        + BlockBodyIndicesProvider
        + TransactionsProvider<Transaction = BaseTxEnvelope>
        + ReceiptProvider<Receipt = BaseReceipt>
        + ChangeSetReader
        + StorageChangeSetReader
        + PruneCheckpointReader
        + Clone
        + Send
        + Sync
        + 'static
{
}

/// Prune segments whose data `archive_getBlockRange` returns; a block pruned in any of them is
/// not exportable.
pub const EXPORTED_PRUNE_SEGMENTS: [PruneSegment; 5] = [
    PruneSegment::SenderRecovery,
    PruneSegment::Receipts,
    PruneSegment::AccountHistory,
    PruneSegment::StorageHistory,
    PruneSegment::Bodies,
];

/// Implementation of the `archive_` namespace over a node provider.
#[derive(Debug, Clone)]
pub struct ArchiveApiImpl<P> {
    /// Node provider.
    pub provider: P,
    /// Range caps.
    pub limits: ArchiveExportLimits,
}

impl<P: ArchiveExportProvider> ArchiveApiImpl<P> {
    /// Creates the endpoint.
    pub const fn new(provider: P, limits: ArchiveExportLimits) -> Self {
        Self { provider, limits }
    }

    /// Computes `archive_status`.
    pub fn status_blocking(&self) -> Result<ArchiveStatus, ArchiveRpcError> {
        Ok(ArchiveStatus {
            head: self.provider.best_block_number().map_err(Self::internal)?,
            safe: self.provider.safe_block_number().map_err(Self::internal)?.unwrap_or_default(),
            finalized: self
                .provider
                .finalized_block_number()
                .map_err(Self::internal)?
                .unwrap_or_default(),
            earliest_local: self.earliest_local()?,
            max_range_blocks: self.limits.max_range_blocks,
            max_range_bytes: self.limits.max_range_bytes,
        })
    }

    /// Earliest block for which every exported table is still present locally.
    pub fn earliest_local(&self) -> Result<u64, ArchiveRpcError> {
        let mut earliest = self.provider.earliest_block_number().map_err(Self::internal)?;
        for segment in EXPORTED_PRUNE_SEGMENTS {
            if let Some(pruned) = self
                .provider
                .get_prune_checkpoint(segment)
                .map_err(Self::internal)?
                .and_then(|checkpoint| checkpoint.block_number)
            {
                earliest = earliest.max(pruned + 1);
            }
        }
        Ok(earliest)
    }

    /// Serves `archive_getBlockRange` with all caps enforced.
    pub fn block_range_blocking(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<ArchiveBlock>, ArchiveRpcError> {
        if from > to {
            return Err(ArchiveRpcError::InvalidRange { from, to });
        }
        let status = self.status_blocking()?;
        let bounds =
            RangeBounds { finalized: status.finalized, earliest_local: status.earliest_local };
        if to > status.finalized {
            return Err(ArchiveRpcError::NotFinalized { to, bounds });
        }
        if from < status.earliest_local {
            return Err(ArchiveRpcError::Pruned { from, bounds });
        }
        let blocks = to - from + 1;
        if blocks > self.limits.max_range_blocks {
            return Err(ArchiveRpcError::TooManyBlocks {
                blocks,
                max: self.limits.max_range_blocks,
            });
        }

        let mut out = Vec::with_capacity(blocks as usize);
        let mut bytes = 0u64;
        for number in from..=to {
            let block = ArchiveBlock::from_data(&self.block_data(number)?);
            bytes += block.payload_len() as u64;
            if bytes > self.limits.max_range_bytes {
                return Err(ArchiveRpcError::TooManyBytes {
                    block: number,
                    max: self.limits.max_range_bytes,
                });
            }
            out.push(block);
        }
        Ok(out)
    }

    /// Reads every exported row of one canonical block.
    pub fn block_data(&self, number: u64) -> Result<BlockData, ArchiveRpcError> {
        let provider = &self.provider;
        let header = provider
            .header_by_number(number)
            .map_err(Self::internal)?
            .ok_or(ArchiveRpcError::NotFound { block: number })?;
        let indices = provider
            .block_body_indices(number)
            .map_err(Self::internal)?
            .ok_or(ArchiveRpcError::NotFound { block: number })?;
        let tx_range = indices.first_tx_num..indices.first_tx_num + indices.tx_count;
        // reth's in-memory provider underflows on an empty range starting at 0 (genesis), and an
        // empty block has no rows to read anyway.
        let (transactions, senders, receipts) = if tx_range.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            (
                provider.transactions_by_tx_range(tx_range.clone()).map_err(Self::internal)?,
                provider.senders_by_tx_range(tx_range.clone()).map_err(Self::internal)?,
                provider.receipts_by_tx_range(tx_range).map_err(Self::internal)?,
            )
        };
        Ok(BlockData::new(
            header,
            BlockRows {
                body_indices: BodyIndices {
                    first_tx_num: indices.first_tx_num,
                    tx_count: indices.tx_count,
                },
                transactions,
                senders,
                receipts,
                account_changes: provider
                    .account_block_changeset(number)
                    .map_err(Self::internal)?,
                storage_changes: provider
                    .storage_block_changeset(number)
                    .map_err(Self::internal)?,
            },
        ))
    }

    /// Maps a provider error.
    pub fn internal(err: impl std::fmt::Display) -> ArchiveRpcError {
        ArchiveRpcError::Internal(err.to_string())
    }
}

#[async_trait]
impl<P: ArchiveExportProvider> ArchiveApiServer for ArchiveApiImpl<P> {
    async fn status(&self) -> RpcResult<ArchiveStatus> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.status_blocking())
            .await
            .map_err(|err| ArchiveRpcError::Internal(err.to_string()))?
            .map_err(Into::into)
    }

    async fn get_block_range(&self, from: u64, to: u64) -> RpcResult<Vec<ArchiveBlock>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.block_range_blocking(from, to))
            .await
            .map_err(|err| ArchiveRpcError::Internal(err.to_string()))?
            .map_err(Into::into)
    }
}
