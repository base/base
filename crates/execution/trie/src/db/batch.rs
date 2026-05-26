//! Batch write session for [`MdbxProofsStorage`] enabling multiple block writes inside one MDBX
//! RW transaction. Reads through the session observe uncommitted writes from earlier blocks in
//! the same session, which is required for cold catch-up where block `N+1` must execute against
//! block `N` written but not yet committed.

use alloy_eips::eip1898::BlockWithParent;
use alloy_primitives::B256;
use reth_db::{Database, DatabaseEnv, transaction::DbTx};

use crate::{
    BaseProofsStorageError, BaseProofsStorageResult, BlockStateDiff,
    api::{BaseProofsBatchSession, WriteCounts},
    db::{
        MdbxProofsStorage, MdbxV2AccountCursor, MdbxV2AccountTrieCursor, MdbxV2StorageCursor,
        MdbxV2StorageTrieCursor,
    },
};

/// Active write batch holding one MDBX RW transaction across multiple block writes.
#[derive(Debug)]
pub struct MdbxBatchSession<'tx> {
    storage: &'tx MdbxProofsStorage,
    tx: Option<<DatabaseEnv as Database>::TXMut>,
}

impl<'tx> MdbxBatchSession<'tx> {
    pub(crate) const fn new(
        storage: &'tx MdbxProofsStorage,
        tx: <DatabaseEnv as Database>::TXMut,
    ) -> Self {
        Self { storage, tx: Some(tx) }
    }

    pub(crate) fn commit(mut self) -> BaseProofsStorageResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit()?;
        }
        Ok(())
    }

    fn tx_ref(&self) -> BaseProofsStorageResult<&<DatabaseEnv as Database>::TXMut> {
        self.tx.as_ref().ok_or(BaseProofsStorageError::BatchSessionClosed)
    }
}

impl BaseProofsBatchSession for MdbxBatchSession<'_> {
    type StorageTrieCursor<'a>
        = MdbxV2StorageTrieCursor
    where
        Self: 'a;
    type AccountTrieCursor<'a>
        = MdbxV2AccountTrieCursor
    where
        Self: 'a;
    type StorageCursor<'a>
        = MdbxV2StorageCursor
    where
        Self: 'a;
    type AccountHashedCursor<'a>
        = MdbxV2AccountCursor
    where
        Self: 'a;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.inner_get_earliest_block_number_hash(self.tx_ref()?)
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.inner_get_latest_block_number_hash(self.tx_ref()?)
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'_>> {
        MdbxV2StorageTrieCursor::new(self.tx_ref()?, max_block_number, hashed_address)
    }

    fn account_trie_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'_>> {
        MdbxV2AccountTrieCursor::new(self.tx_ref()?, max_block_number)
    }

    fn storage_hashed_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'_>> {
        MdbxV2StorageCursor::new(self.tx_ref()?, max_block_number, hashed_address)
    }

    fn account_hashed_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'_>> {
        MdbxV2AccountCursor::new(self.tx_ref()?, max_block_number)
    }

    fn store_trie_updates(
        &mut self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.storage.store_trie_updates_append_only(self.tx_ref()?, block_ref, block_state_diff)
    }
}
