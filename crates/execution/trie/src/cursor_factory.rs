//! Implements [`TrieCursorFactory`] and [`HashedCursorFactory`] for [`BaseProofsStore`] types.

use std::marker::PhantomData;

use alloy_primitives::B256;
use reth_db::DatabaseError;
use reth_trie::{hashed_cursor::HashedCursorFactory, trie_cursor::TrieCursorFactory};

use crate::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsStorage,
    BaseProofsStore, BaseProofsTrieCursor,
};

/// Factory for creating trie cursors for [`BaseProofsStore`].
#[derive(Debug, Clone)]
pub struct BaseProofsTrieCursorFactory<'tx, S: BaseProofsStore> {
    storage: &'tx BaseProofsStorage<S>,
    block_number: u64,
    _marker: PhantomData<&'tx ()>,
}

impl<'tx, S: BaseProofsStore> BaseProofsTrieCursorFactory<'tx, S> {
    /// Initializes new `BaseProofsTrieCursorFactory`
    pub const fn new(storage: &'tx BaseProofsStorage<S>, block_number: u64) -> Self {
        Self { storage, block_number, _marker: PhantomData }
    }
}

impl<'tx, S> TrieCursorFactory for BaseProofsTrieCursorFactory<'tx, S>
where
    for<'a> S: BaseProofsStore + 'tx,
{
    type AccountTrieCursor<'a>
        = BaseProofsTrieCursor<S::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = BaseProofsTrieCursor<S::StorageTrieCursor<'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(BaseProofsTrieCursor::new(
            self.storage
                .account_trie_cursor(self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(BaseProofsTrieCursor::new(
            self.storage
                .storage_trie_cursor(hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Factory for creating hashed account cursors for [`BaseProofsStore`].
#[derive(Debug, Clone)]
pub struct BaseProofsHashedAccountCursorFactory<'tx, S: BaseProofsStore> {
    storage: &'tx BaseProofsStorage<S>,
    block_number: u64,
    _marker: PhantomData<&'tx ()>,
}

impl<'tx, S: BaseProofsStore> BaseProofsHashedAccountCursorFactory<'tx, S> {
    /// Creates a new `BaseProofsHashedAccountCursorFactory` instance.
    pub const fn new(storage: &'tx BaseProofsStorage<S>, block_number: u64) -> Self {
        Self { storage, block_number, _marker: PhantomData }
    }
}

impl<'tx, S> HashedCursorFactory for BaseProofsHashedAccountCursorFactory<'tx, S>
where
    S: BaseProofsStore + 'tx,
{
    type AccountCursor<'a>
        = BaseProofsHashedAccountCursor<S::AccountHashedCursor<'a>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = BaseProofsHashedStorageCursor<S::StorageCursor<'a>>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(BaseProofsHashedAccountCursor::new(
            self.storage.account_hashed_cursor(self.block_number)?,
        ))
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(BaseProofsHashedStorageCursor::new(
            self.storage.storage_hashed_cursor(hashed_address, self.block_number)?,
        ))
    }
}
