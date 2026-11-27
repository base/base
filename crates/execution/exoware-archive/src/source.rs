//! Local block data the exporter reads, and the node state the reader falls back to.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use base_common_consensus::BaseReceipt;

use crate::{
    AccountChange, AccountInfo, ArchiveConfig, ArchiveError, BlockBody, BlockData, BlockVerifier,
    StorageChange,
};

/// Lowest block each local table still holds (0 when nothing is pruned).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalAvailability {
    /// Headers.
    pub headers: u64,
    /// Bodies (transactions).
    pub bodies: u64,
    /// Receipts.
    pub receipts: u64,
    /// Senders. When pruned, senders are recovered from signatures instead.
    pub senders: u64,
    /// Account changesets.
    pub account_changes: u64,
    /// Storage changesets.
    pub storage_changes: u64,
}

impl LocalAvailability {
    /// The first table, other than senders, that no longer holds `block`.
    pub const fn first_missing(&self, block: u64) -> Option<(&'static str, u64)> {
        if self.headers > block {
            return Some(("headers", self.headers));
        }
        if self.bodies > block {
            return Some(("bodies", self.bodies));
        }
        if self.receipts > block {
            return Some(("receipts", self.receipts));
        }
        if self.account_changes > block {
            return Some(("account changesets", self.account_changes));
        }
        if self.storage_changes > block {
            return Some(("storage changesets", self.storage_changes));
        }
        None
    }
}

/// The node's local block data, read synchronously on the blocking pool.
///
/// A reth-provider implementation maps each method onto one provider call: headers, block
/// bodies, receipts and senders by number, account and storage changesets by block, the
/// finalized block number, and the prune checkpoints for [`Self::availability`]. A pruned
/// changeset can read as empty, so the exporter checks availability before and after reading.
#[cfg_attr(test, mockall::automock)]
pub trait BlockSource: Send + Sync + 'static {
    /// Finalized block number, if any.
    fn finalized_block_number(&self) -> Result<Option<u64>, ArchiveError>;

    /// Lowest block each table still holds.
    fn availability(&self) -> Result<LocalAvailability, ArchiveError>;

    /// Header of block `number`.
    fn header(&self, number: u64) -> Result<Option<Header>, ArchiveError>;

    /// Body of block `number`.
    fn body(&self, number: u64) -> Result<Option<BlockBody>, ArchiveError>;

    /// Receipts of block `number`.
    fn receipts(&self, number: u64) -> Result<Option<Vec<BaseReceipt>>, ArchiveError>;

    /// Senders of block `number`, `None` when the sender table does not have them.
    fn senders(&self, number: u64) -> Result<Option<Vec<Address>>, ArchiveError>;

    /// Account before-values of block `number`.
    fn account_changes(&self, number: u64) -> Result<Vec<AccountChange>, ArchiveError>;

    /// Storage before-values of block `number`.
    fn storage_changes(&self, number: u64) -> Result<Vec<StorageChange>, ArchiveError>;
}

/// Reads one segment's blocks from a [`BlockSource`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentLoader;

impl SegmentLoader {
    /// Reads every block of `segment`, failing with [`ArchiveError::LocalDataPruned`] when any
    /// table lost a block before or during the read. Senders are recovered when the sender
    /// table does not have them.
    pub fn load<B: BlockSource + ?Sized>(
        source: &B,
        config: &ArchiveConfig,
        segment: u64,
    ) -> Result<Vec<BlockData>, ArchiveError> {
        Self::load_range(source, config.first_block(segment), config.last_block(segment))
    }

    /// Reads blocks `first..=last`, checking availability before and after the read as
    /// [`Self::load`] does. The exporter reads a segment in such bounded batches.
    pub fn load_range<B: BlockSource + ?Sized>(
        source: &B,
        first: u64,
        last: u64,
    ) -> Result<Vec<BlockData>, ArchiveError> {
        Self::check_available(&source.availability()?, first)?;
        let missing = |table: &'static str, block: u64| ArchiveError::LocalDataPruned {
            table,
            block,
            earliest: block.saturating_add(1),
        };
        let mut blocks = Vec::with_capacity(last.saturating_sub(first).saturating_add(1) as usize);
        for number in first..=last {
            let header = source.header(number)?.ok_or_else(|| missing("headers", number))?;
            let body = source.body(number)?.ok_or_else(|| missing("bodies", number))?;
            let receipts = source.receipts(number)?.ok_or_else(|| missing("receipts", number))?;
            let senders = match source.senders(number)? {
                Some(senders) if senders.len() == body.transactions.len() => senders,
                _ => BlockVerifier::recover_senders(number, &body.transactions)?,
            };
            blocks.push(BlockData {
                header,
                body,
                senders,
                receipts,
                account_changes: source.account_changes(number)?,
                storage_changes: source.storage_changes(number)?,
            });
        }
        Self::check_available(&source.availability()?, first)?;
        Ok(blocks)
    }

    /// Fails when a table no longer holds `block`.
    pub const fn check_available(
        availability: &LocalAvailability,
        block: u64,
    ) -> Result<(), ArchiveError> {
        match availability.first_missing(block) {
            Some((table, earliest)) => {
                Err(ArchiveError::LocalDataPruned { table, block, earliest })
            }
            None => Ok(()),
        }
    }
}

/// The node's state and canonical chain, consulted by the reader for answers the archive
/// delegates (state after the highest archived block, bytecode, canonical hashes).
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait NodeState: Send + Sync {
    /// Canonical hash of block `number`, if the node still has its header.
    async fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ArchiveError>;

    /// Account at the end of block `block` (`history_by_block_number(block)`).
    async fn account_at(
        &self,
        block: u64,
        address: Address,
    ) -> Result<Option<AccountInfo>, ArchiveError>;

    /// Storage slot at the end of block `block`.
    async fn storage_at(
        &self,
        block: u64,
        address: Address,
        slot: B256,
    ) -> Result<U256, ArchiveError>;

    /// Bytecode with hash `code_hash`; bytecodes are never pruned.
    async fn bytecode(&self, code_hash: B256) -> Result<Option<Bytes>, ArchiveError>;
}
