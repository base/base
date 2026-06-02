//! Batch write session for [`MdbxProofsStorage`] enabling multiple block writes inside one MDBX
//! RW transaction. Reads through the session observe uncommitted writes from earlier blocks in
//! the same session, which is required for cold catch-up where block `N+1` must execute against
//! block `N` written but not yet committed.

use alloy_eips::eip1898::BlockWithParent;
use alloy_primitives::{B256, U256};
use reth_db::{Database, DatabaseEnv, transaction::DbTx};
use reth_primitives_traits::Account;

use crate::{
    BaseProofsStorageError, BaseProofsStorageResult, BlockStateDiff,
    api::{BaseProofsBatchSession, WriteCounts},
    db::{
        MdbxProofsStorage, MdbxV2AccountCursor, MdbxV2AccountCursorEither, MdbxV2AccountTrieCursor,
        MdbxV2AccountTrieCursorEither, MdbxV2LatestAccountCursor, MdbxV2LatestAccountTrieCursor,
        MdbxV2LatestStorageCursor, MdbxV2LatestStorageTrieCursor, MdbxV2StorageCursor,
        MdbxV2StorageCursorEither, MdbxV2StorageTrieCursor, MdbxV2StorageTrieCursorEither,
        V2AccountsTrie, V2HashedAccounts, V2HashedStorages, V2StoragesTrie,
    },
};

type TxMut = <DatabaseEnv as Database>::TXMut;
type V2AccountTrieCursor = <TxMut as DbTx>::Cursor<V2AccountsTrie>;
type V2StorageTrieCursor = <TxMut as DbTx>::DupCursor<V2StoragesTrie>;
type V2AccountCursor = <TxMut as DbTx>::Cursor<V2HashedAccounts>;
type V2StorageCursor = <TxMut as DbTx>::DupCursor<V2HashedStorages>;

/// Active write batch holding one MDBX RW transaction across multiple block writes.
#[derive(Debug)]
pub struct MdbxBatchSession<'tx> {
    storage: &'tx MdbxProofsStorage,
    tx: Option<TxMut>,
}

impl<'tx> MdbxBatchSession<'tx> {
    pub(crate) const fn new(storage: &'tx MdbxProofsStorage, tx: TxMut) -> Self {
        Self { storage, tx: Some(tx) }
    }

    pub(crate) fn commit(mut self) -> BaseProofsStorageResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit()?;
        }
        Ok(())
    }

    fn tx_ref(&self) -> BaseProofsStorageResult<&TxMut> {
        self.tx.as_ref().ok_or(BaseProofsStorageError::BatchSessionClosed)
    }

    fn should_use_latest_cursor(&self, max_block_number: u64) -> BaseProofsStorageResult<bool> {
        Ok(self
            .get_latest_block_number()?
            .is_some_and(|(latest_block_number, _)| latest_block_number == max_block_number))
    }
}

impl BaseProofsBatchSession for MdbxBatchSession<'_> {
    type StorageTrieCursor<'a>
        = MdbxV2StorageTrieCursorEither<V2StorageTrieCursor>
    where
        Self: 'a;
    type AccountTrieCursor<'a>
        = MdbxV2AccountTrieCursorEither<V2AccountTrieCursor>
    where
        Self: 'a;
    type StorageCursor<'a>
        = MdbxV2StorageCursorEither<V2StorageCursor>
    where
        Self: 'a;
    type AccountHashedCursor<'a>
        = MdbxV2AccountCursorEither<V2AccountCursor>
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
        if self.should_use_latest_cursor(max_block_number)? {
            return Ok(MdbxV2StorageTrieCursorEither::Latest(MdbxV2LatestStorageTrieCursor::new(
                self.tx_ref()?.cursor_dup_read::<V2StoragesTrie>()?,
                hashed_address,
            )));
        }
        Ok(MdbxV2StorageTrieCursorEither::Historical(MdbxV2StorageTrieCursor::new(
            self.tx_ref()?,
            max_block_number,
            hashed_address,
        )?))
    }

    fn account_trie_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'_>> {
        if self.should_use_latest_cursor(max_block_number)? {
            return Ok(MdbxV2AccountTrieCursorEither::Latest(MdbxV2LatestAccountTrieCursor::new(
                self.tx_ref()?.cursor_read::<V2AccountsTrie>()?,
            )));
        }
        Ok(MdbxV2AccountTrieCursorEither::Historical(MdbxV2AccountTrieCursor::new(
            self.tx_ref()?,
            max_block_number,
        )?))
    }

    fn storage_hashed_cursor(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'_>> {
        if self.should_use_latest_cursor(max_block_number)? {
            return Ok(MdbxV2StorageCursorEither::Latest(MdbxV2LatestStorageCursor::new(
                self.tx_ref()?.cursor_dup_read::<V2HashedStorages>()?,
                hashed_address,
            )));
        }
        Ok(MdbxV2StorageCursorEither::Historical(MdbxV2StorageCursor::new(
            self.tx_ref()?,
            max_block_number,
            hashed_address,
        )?))
    }

    fn account_hashed_cursor(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'_>> {
        if self.should_use_latest_cursor(max_block_number)? {
            return Ok(MdbxV2AccountCursorEither::Latest(MdbxV2LatestAccountCursor::new(
                self.tx_ref()?.cursor_read::<V2HashedAccounts>()?,
            )));
        }
        Ok(MdbxV2AccountCursorEither::Historical(MdbxV2AccountCursor::new(
            self.tx_ref()?,
            max_block_number,
        )?))
    }

    fn hashed_account(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<Account>> {
        self.storage.inner_hashed_account(self.tx_ref()?, hashed_address, max_block_number)
    }

    fn hashed_storage(
        &self,
        hashed_address: B256,
        hashed_storage_key: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<U256>> {
        self.storage.inner_hashed_storage(
            self.tx_ref()?,
            hashed_address,
            hashed_storage_key,
            max_block_number,
        )
    }

    fn store_trie_updates(
        &mut self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.storage.store_trie_updates_append_only(self.tx_ref()?, block_ref, block_state_diff)
    }
}
