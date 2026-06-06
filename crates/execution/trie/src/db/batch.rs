//! Batch write session for [`MdbxProofsStorage`] enabling multiple block writes inside one MDBX
//! RW transaction. Reads through the session observe uncommitted writes from earlier blocks in
//! the same session, which is required for cold catch-up where block `N+1` must execute against
//! block `N` written but not yet committed.

use alloy_eips::eip1898::BlockWithParent;
use alloy_primitives::B256;
use reth_db::{
    Database, DatabaseEnv,
    table::{DupSort, Table},
    transaction::DbTx,
};

use crate::{
    BaseProofsStorageError, BaseProofsStorageResult, BlockStateDiff,
    api::{BaseProofsBatchSession, WriteCounts},
    db::{
        AccountTrieHistory, HashedAccountHistory, HashedStorageHistory, MdbxAccountCursor,
        MdbxProofsStorage, MdbxStorageCursor, MdbxTrieCursor, StorageTrieHistory,
    },
};

/// Alias for the dup-sorted cursor type produced by an MDBX RW transaction.
pub type DupRw<'tx, T> = <<DatabaseEnv as Database>::TXMut as DbTx>::DupCursor<T>;

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

    fn dup_cursor<T: Table + DupSort>(&self) -> BaseProofsStorageResult<DupRw<'_, T>> {
        Ok(self.tx_ref()?.cursor_dup_read::<T>()?)
    }
}

impl BaseProofsBatchSession for MdbxBatchSession<'_> {
    type StorageTrieCursor<'a>
        = MdbxTrieCursor<StorageTrieHistory, DupRw<'a, StorageTrieHistory>>
    where
        Self: 'a;
    type AccountTrieCursor<'a>
        = MdbxTrieCursor<AccountTrieHistory, DupRw<'a, AccountTrieHistory>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = MdbxStorageCursor<DupRw<'a, HashedStorageHistory>>
    where
        Self: 'a;
    type AccountHashedCursor<'a>
        = MdbxAccountCursor<DupRw<'a, HashedAccountHistory>>
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
        Ok(MdbxTrieCursor::new(
            self.dup_cursor::<StorageTrieHistory>()?,
            max_block_number,
            Some(hashed_address),
        ))
    }

    fn account_trie_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'_>> {
        Ok(MdbxTrieCursor::new(self.dup_cursor::<AccountTrieHistory>()?, max_block_number, None))
    }

    fn storage_hashed_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'_>> {
        Ok(MdbxStorageCursor::new(
            self.dup_cursor::<HashedStorageHistory>()?,
            max_block_number,
            hashed_address,
        ))
    }

    fn account_hashed_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'_>> {
        Ok(MdbxAccountCursor::new(self.dup_cursor::<HashedAccountHistory>()?, max_block_number))
    }

    fn store_trie_updates(
        &mut self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.storage.store_trie_updates_append_only(self.tx_ref()?, block_ref, block_state_diff)
    }
}
