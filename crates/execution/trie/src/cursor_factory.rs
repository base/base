//! Implements [`TrieCursorFactory`] and [`HashedCursorFactory`] for [`BaseProofsStore`] types.
//!
//! Both factories borrow one read-only [`BaseProofsStore::Tx`] for their entire lifetime
//! and route every cursor allocation through the `*_with_tx` fast path. This mirrors
//! reth's own `DatabaseTrieCursorFactory` / `DatabaseHashedCursorFactory` pattern and is
//! what lets proof, state-root, and witness requests acquire exactly one MDBX
//! transaction.

use alloy_primitives::B256;
use reth_db::DatabaseError;
use reth_trie::{hashed_cursor::HashedCursorFactory, trie_cursor::TrieCursorFactory};

use crate::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsStorage,
    BaseProofsStore, BaseProofsTrieCursor,
};

/// Request-scoped factory that opens trie cursors against a shared read-only transaction.
///
/// Holds a borrow of the transaction so every cursor allocation reuses the same MDBX
/// reader slot. See [`BaseProofsStore::Tx`] for the underlying contention story.
#[derive(Debug, Clone)]
pub struct BaseProofsTrieCursorFactory<'tx, S: BaseProofsStore> {
    storage: &'tx BaseProofsStorage<S>,
    tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx,
    block_number: u64,
}

impl<'tx, S: BaseProofsStore> BaseProofsTrieCursorFactory<'tx, S> {
    /// Initializes a request-scoped trie cursor factory bound to `tx`.
    pub const fn new(
        storage: &'tx BaseProofsStorage<S>,
        tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx,
        block_number: u64,
    ) -> Self {
        Self { storage, tx, block_number }
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
                .account_trie_cursor_with_tx(self.tx, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(BaseProofsTrieCursor::new(
            self.storage
                .storage_trie_cursor_with_tx(self.tx, hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Request-scoped factory that opens hashed cursors against a shared read-only transaction.
///
/// Mirror of [`BaseProofsTrieCursorFactory`] for the hashed account/storage tries.
#[derive(Debug, Clone)]
pub struct BaseProofsHashedAccountCursorFactory<'tx, S: BaseProofsStore> {
    storage: &'tx BaseProofsStorage<S>,
    tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx,
    block_number: u64,
}

impl<'tx, S: BaseProofsStore> BaseProofsHashedAccountCursorFactory<'tx, S> {
    /// Initializes a request-scoped hashed cursor factory bound to `tx`.
    pub const fn new(
        storage: &'tx BaseProofsStorage<S>,
        tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx,
        block_number: u64,
    ) -> Self {
        Self { storage, tx, block_number }
    }
}

impl<'tx, S> HashedCursorFactory for BaseProofsHashedAccountCursorFactory<'tx, S>
where
    for<'a> S: BaseProofsStore + 'tx,
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
            self.storage.account_hashed_cursor_with_tx(self.tx, self.block_number)?,
        ))
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(BaseProofsHashedStorageCursor::new(self.storage.storage_hashed_cursor_with_tx(
            self.tx,
            hashed_address,
            self.block_number,
        )?))
    }
}
