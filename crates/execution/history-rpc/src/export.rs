//! `archive_` export endpoint served from the node's own provider (TDD §3).

use std::sync::Arc;

use alloy_consensus::{Header, transaction::SignerRecoverable};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_history_store::{
    ArchiveApiServer, ArchiveBlock, ArchiveRpcError, ArchiveStatus, BlockData, BlockRows,
    BodyIndices, RangeBounds,
};
use jsonrpsee::core::{RpcResult, async_trait};
use reth_prune_types::PruneSegment;
use reth_storage_api::{
    BlockBodyIndicesProvider, BlockIdReader, BlockNumReader, ChangeSetReader, HeaderProvider,
    PruneCheckpointReader, ReceiptProvider, StorageChangeSetReader, TransactionsProvider,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default number of `archive_getBlockRange` calls one node serves at a time.
pub const DEFAULT_MAX_CONCURRENT_RANGES: usize = 2;

/// Range caps of the export endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveExportLimits {
    /// Maximum blocks per `archive_getBlockRange` call.
    pub max_range_blocks: u64,
    /// Maximum payload bytes per `archive_getBlockRange` call.
    pub max_range_bytes: u64,
    /// Maximum `archive_getBlockRange` calls served at once; further calls fail with
    /// [`ArchiveRpcError::Busy`] instead of queueing.
    pub max_concurrent_ranges: usize,
}

/// Concurrency limit of `archive_getBlockRange`. A range call reads up to
/// `maxRangeBytes` from the node's database on the blocking pool, so a bounded number run at
/// once and the rest are refused immediately; writers move on to another node or back off.
#[derive(Debug, Clone)]
pub struct RangePermits {
    /// Free slots.
    pub semaphore: Arc<Semaphore>,
    /// Configured limit, echoed in [`ArchiveRpcError::Busy`].
    pub max: usize,
}

impl RangePermits {
    /// Creates a limit of `max` concurrent ranges.
    pub fn new(max: usize) -> Self {
        Self { semaphore: Arc::new(Semaphore::new(max)), max }
    }

    /// Takes a slot for one range call, or fails with [`ArchiveRpcError::Busy`] when all are
    /// taken. The slot is released when the permit drops.
    pub fn try_acquire(&self) -> Result<OwnedSemaphorePermit, ArchiveRpcError> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map_err(|_| ArchiveRpcError::Busy { max: self.max })
    }
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
/// not exportable. `SenderRecovery` is absent: pruned senders are recovered from the
/// transactions instead ([`SenderSource::Recover`]), so a node pruning them (reth's `--full`)
/// still serves ranges.
pub const EXPORTED_PRUNE_SEGMENTS: [PruneSegment; 4] = [
    PruneSegment::Receipts,
    PruneSegment::AccountHistory,
    PruneSegment::StorageHistory,
    PruneSegment::Bodies,
];

/// Where a range export takes transaction senders from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderSource {
    /// The node's sender table, which holds every block in the range.
    Table,
    /// Recovered from each transaction's signature, because the node prunes senders fully
    /// (reth then never writes them) or the `SenderRecovery` prune checkpoint covers a block in
    /// the range. Deposits yield their `from`, as in the writer's `ChunkVerifier`.
    Recover,
}

/// Implementation of the `archive_` namespace over a node provider.
#[derive(Debug, Clone)]
pub struct ArchiveApiImpl<P> {
    /// Node provider.
    pub provider: P,
    /// Range caps.
    pub limits: ArchiveExportLimits,
    /// Concurrency limit of range calls.
    pub permits: RangePermits,
}

impl<P: ArchiveExportProvider> ArchiveApiImpl<P> {
    /// Creates the endpoint.
    pub fn new(provider: P, limits: ArchiveExportLimits) -> Self {
        Self { provider, limits, permits: RangePermits::new(limits.max_concurrent_ranges) }
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

        let senders = self.sender_source(from)?;
        let mut out = Vec::with_capacity(blocks as usize);
        let mut bytes = 0u64;
        for number in from..=to {
            let block = ArchiveBlock::from_data(&self.block_data(number, senders)?);
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

    /// Chooses where a range starting at `from` takes its senders: recovered when the
    /// `SenderRecovery` prune checkpoint is in `Full` mode (reth skips writing senders
    /// altogether, whatever block the checkpoint names) or is at or above `from`, otherwise
    /// the sender table.
    pub fn sender_source(&self, from: u64) -> Result<SenderSource, ArchiveRpcError> {
        let checkpoint = self
            .provider
            .get_prune_checkpoint(PruneSegment::SenderRecovery)
            .map_err(Self::internal)?;
        Ok(
            if checkpoint.is_some_and(|checkpoint| {
                checkpoint.prune_mode.is_full()
                    || checkpoint.block_number.is_some_and(|pruned| pruned >= from)
            }) {
                SenderSource::Recover
            } else {
                SenderSource::Table
            },
        )
    }

    /// Reads every exported row of one canonical block, taking senders from `senders`.
    /// Recovery is CPU work on the calling thread, which for range calls is the blocking pool.
    pub fn block_data(
        &self,
        number: u64,
        senders: SenderSource,
    ) -> Result<BlockData, ArchiveRpcError> {
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
            let transactions =
                provider.transactions_by_tx_range(tx_range.clone()).map_err(Self::internal)?;
            let table = match senders {
                SenderSource::Table => {
                    provider.senders_by_tx_range(tx_range.clone()).map_err(Self::internal)?
                }
                SenderSource::Recover => Vec::new(),
            };
            // A full sender-recovery node writes no senders even before its first prune run
            // records a checkpoint, and a prune can race this read: recover whenever the
            // table does not hold every sender.
            let senders = if table.len() == transactions.len() {
                table
            } else {
                transactions
                    .iter()
                    .map(|tx| tx.recover_signer().map_err(Self::internal))
                    .collect::<Result<_, _>>()?
            };
            (
                transactions,
                senders,
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
        let permit = self.permits.try_acquire()?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            this.block_range_blocking(from, to)
        })
        .await
        .map_err(|err| ArchiveRpcError::Internal(err.to_string()))?
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With every slot held, a further range call is refused as busy (`4008`); releasing a slot
    /// admits the next one.
    #[test]
    fn range_calls_beyond_the_limit_are_busy() {
        let permits = RangePermits::new(2);
        let first = permits.try_acquire().expect("first slot");
        let _second = permits.try_acquire().expect("second slot");
        let err = permits.try_acquire().expect_err("third call while two are in flight");
        assert_eq!(err, ArchiveRpcError::Busy { max: 2 }, "busy names the limit");
        assert_eq!(err.code(), ArchiveRpcError::BUSY, "busy has its own code");
        assert_eq!(ArchiveRpcError::BUSY, 4008, "busy follows the 4001-4007 range codes");
        drop(first);
        assert!(permits.try_acquire().is_ok(), "a released slot is reused");
    }
}
