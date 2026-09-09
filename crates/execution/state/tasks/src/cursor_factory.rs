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
    api::BaseProofsBatchSession,
    cursor::{
        BaseProofsHashedAccountCursor as RawHashedAccountCursor,
        BaseProofsHashedStorageCursor as RawHashedStorageCursor,
        BaseProofsTrieCursor as RawTrieCursor,
    },
};

/// Request-scoped factory that opens trie cursors against a shared read-only transaction.
///
/// Holds a borrow of the transaction so every cursor allocation reuses the same MDBX
/// reader slot. See [`BaseProofsStore::Tx`] for the underlying contention story.
#[derive(Debug, Clone)]
pub struct BaseProofsTrieCursorFactory<'tx, 'db, S: BaseProofsStore> {
    storage: &'db BaseProofsStorage<S>,
    tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx<'db>,
    block_number: u64,
}

impl<'tx, 'db, S: BaseProofsStore> BaseProofsTrieCursorFactory<'tx, 'db, S> {
    /// Initializes a request-scoped trie cursor factory bound to `tx`.
    pub const fn new(
        storage: &'db BaseProofsStorage<S>,
        tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx<'db>,
        block_number: u64,
    ) -> Self {
        Self { storage, tx, block_number }
    }
}

impl<'tx, 'db, S> TrieCursorFactory for BaseProofsTrieCursorFactory<'tx, 'db, S>
where
    for<'a> S: BaseProofsStore + 'db,
    'db: 'tx,
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
pub struct BaseProofsHashedAccountCursorFactory<'tx, 'db, S: BaseProofsStore> {
    storage: &'db BaseProofsStorage<S>,
    tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx<'db>,
    block_number: u64,
}

impl<'tx, 'db, S: BaseProofsStore> BaseProofsHashedAccountCursorFactory<'tx, 'db, S> {
    /// Initializes a request-scoped hashed cursor factory bound to `tx`.
    pub const fn new(
        storage: &'db BaseProofsStorage<S>,
        tx: &'tx <BaseProofsStorage<S> as BaseProofsStore>::Tx<'db>,
        block_number: u64,
    ) -> Self {
        Self { storage, tx, block_number }
    }
}

impl<'tx, 'db, S> HashedCursorFactory for BaseProofsHashedAccountCursorFactory<'tx, 'db, S>
where
    for<'a> S: BaseProofsStore + 'db,
    'db: 'tx,
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

/// Session-scoped trie cursor factory backed by a [`BaseProofsBatchSession`].
///
/// Cursors read from the session's active transaction and therefore observe writes
/// from earlier `store_trie_updates` calls in the same session.
#[derive(Debug)]
pub struct BaseProofsBatchTrieCursorFactory<'a, S: BaseProofsBatchSession> {
    session: &'a S,
    block_number: u64,
}

impl<S: BaseProofsBatchSession> Clone for BaseProofsBatchTrieCursorFactory<'_, S> {
    fn clone(&self) -> Self {
        Self { session: self.session, block_number: self.block_number }
    }
}

impl<'a, S: BaseProofsBatchSession> BaseProofsBatchTrieCursorFactory<'a, S> {
    /// Initializes a session-scoped trie cursor factory.
    pub const fn new(session: &'a S, block_number: u64) -> Self {
        Self { session, block_number }
    }
}

impl<S> TrieCursorFactory for BaseProofsBatchTrieCursorFactory<'_, S>
where
    S: BaseProofsBatchSession,
{
    type AccountTrieCursor<'a>
        = RawTrieCursor<S::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = RawTrieCursor<S::StorageTrieCursor<'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(RawTrieCursor::new(
            self.session
                .account_trie_cursor(self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(RawTrieCursor::new(
            self.session
                .storage_trie_cursor(hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Session-scoped hashed cursor factory backed by a [`BaseProofsBatchSession`].
#[derive(Debug)]
pub struct BaseProofsBatchHashedAccountCursorFactory<'a, S: BaseProofsBatchSession> {
    session: &'a S,
    block_number: u64,
}

impl<S: BaseProofsBatchSession> Clone for BaseProofsBatchHashedAccountCursorFactory<'_, S> {
    fn clone(&self) -> Self {
        Self { session: self.session, block_number: self.block_number }
    }
}

impl<'a, S: BaseProofsBatchSession> BaseProofsBatchHashedAccountCursorFactory<'a, S> {
    /// Initializes a session-scoped hashed cursor factory.
    pub const fn new(session: &'a S, block_number: u64) -> Self {
        Self { session, block_number }
    }
}

impl<S> HashedCursorFactory for BaseProofsBatchHashedAccountCursorFactory<'_, S>
where
    S: BaseProofsBatchSession,
{
    type AccountCursor<'a>
        = RawHashedAccountCursor<S::AccountHashedCursor<'a>>
    where
        Self: 'a;
    type StorageCursor<'a>
        = RawHashedStorageCursor<S::StorageCursor<'a>>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(RawHashedAccountCursor::new(self.session.account_hashed_cursor(self.block_number)?))
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(RawHashedStorageCursor::new(
            self.session.storage_hashed_cursor(hashed_address, self.block_number)?,
        ))
    }
}
