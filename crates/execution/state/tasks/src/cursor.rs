//! Implementation of [`HashedCursor`] and [`TrieCursor`] for
//! [`BaseProofsStorage`](crate::BaseProofsStorage).

use alloy_primitives::{B256, U256};
use base_execution_state_memory::StoredAccount as Account;
use base_execution_state_types::{BranchNodeCompact, Nibbles};
use derive_more::Constructor;
use base_execution_state_database::DatabaseError;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};

/// Manages reading storage or account trie nodes from [`TrieCursor`].
#[derive(Debug, Clone, Constructor)]
pub struct BaseProofsTrieCursor<C>(pub C);

impl<C> TrieCursor for BaseProofsTrieCursor<C>
where
    C: TrieCursor,
{
    #[inline]
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.seek_exact(key)
    }

    #[inline]
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        self.0.current()
    }

    #[inline]
    fn reset(&mut self) {
        self.0.reset()
    }
}

impl<C> TrieStorageCursor for BaseProofsTrieCursor<C>
where
    C: TrieStorageCursor,
{
    #[inline]
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.0.set_hashed_address(hashed_address)
    }
}

/// Manages reading hashed account nodes from external storage.
#[derive(Debug, Clone, Constructor)]
pub struct BaseProofsHashedAccountCursor<C>(pub C);

impl<C> HashedCursor for BaseProofsHashedAccountCursor<C>
where
    C: HashedCursor<Value = Account> + Send + Sync,
{
    type Value = Account;

    #[inline]
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Manages reading hashed storage nodes from external storage.
#[derive(Debug, Clone, Constructor)]
pub struct BaseProofsHashedStorageCursor<C>(pub C);

impl<C> HashedCursor for BaseProofsHashedStorageCursor<C>
where
    C: HashedCursor<Value = U256> + Send + Sync,
{
    type Value = U256;

    #[inline]
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    fn reset(&mut self) {
        self.0.reset()
    }
}

impl<C> HashedStorageCursor for BaseProofsHashedStorageCursor<C>
where
    C: HashedStorageCursor<Value = U256> + Send + Sync,
{
    #[inline]
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        self.0.is_storage_empty()
    }

    #[inline]
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.0.set_hashed_address(hashed_address)
    }
}
