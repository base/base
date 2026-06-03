use std::marker::PhantomData;

use alloy_primitives::{B256, U256};
use reth_db::{
    Database, DatabaseEnv, DatabaseError,
    cursor::{DbCursorRO, DbDupCursorRO},
    table::{DupSort, Table},
    transaction::DbTx,
};

/// Generic alias for dup cursor for T
pub type Dup<'tx, T> = <<DatabaseEnv as Database>::TX as DbTx>::DupCursor<T>;
use reth_primitives_traits::{Account, StorageEntry};
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles, StoredNibbles, StoredNibblesSubKey};

use crate::{
    BaseProofsStorageResult,
    db::{
        AccountTrieHistory, AccountTrieShardedKey, BlockNumberHashedAddress, HashedAccountHistory,
        HashedAccountShardedKey, HashedStorageHistory, HashedStorageKey, HashedStorageShardedKey,
        MaybeDeleted, StorageTrieHistory, StorageTrieKey, StorageTrieShardedKey,
        V2AccountTrieChangeSets, V2AccountsTrie, V2AccountsTrieHistory, V2HashedAccountChangeSets,
        V2HashedAccounts, V2HashedAccountsHistory, V2HashedStorageChangeSets, V2HashedStorages,
        V2HashedStoragesHistory, V2StorageTrieChangeSets, V2StoragesTrie, V2StoragesTrieHistory,
        VersionedValue,
    },
};

/// Iterates versioned dup-sorted rows and returns the latest value (<= `max_block_number`),
/// skipping tombstones.
#[derive(Debug, Clone)]
pub struct BlockNumberVersionedCursor<T: Table + DupSort, Cursor> {
    _table: PhantomData<T>,
    cursor: Cursor,
    max_block_number: u64,
}

impl<V, T, Cursor> BlockNumberVersionedCursor<T, Cursor>
where
    T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
    Cursor: DbCursorRO<T> + DbDupCursorRO<T>,
{
    /// Initializes new [`BlockNumberVersionedCursor`].
    pub const fn new(cursor: Cursor, max_block_number: u64) -> Self {
        Self { _table: PhantomData, cursor, max_block_number }
    }

    /// Check if the cursor is currently positioned at a valid row.
    fn is_positioned(&mut self) -> BaseProofsStorageResult<bool> {
        Ok(self.cursor.current()?.is_some())
    }

    /// Resolve the latest version for `key` with `block_number` <= `max_block_number`.
    /// Strategy:
    /// - `seek_by_key_subkey(key, max)` gives first dup >= max.
    ///   - if exactly == max → it's our latest
    ///   - if > max → `prev_dup()` is latest < max (or None)
    /// - if no dup >= max:
    ///   - if key exists → `last_dup()` is latest < max
    ///   - else → None
    fn latest_version_for_key(
        &mut self,
        key: T::Key,
    ) -> BaseProofsStorageResult<Option<(T::Key, T::Value)>> {
        // First dup with subkey >= max_block_number
        let seek_res = self.cursor.seek_by_key_subkey(key.clone(), self.max_block_number)?;

        if let Some(vv) = seek_res {
            if vv.block_number > self.max_block_number {
                // step back to the last dup < max
                return Ok(self.cursor.prev_dup()?);
            }
            // already at the dup = max
            return Ok(Some((key, vv)));
        }

        // No dup >= max ⇒ either key absent or all dups < max. Check if key exists:
        if self.cursor.seek_exact(key.clone())?.is_none() {
            return Ok(None);
        }

        // Key exists ⇒ take last dup (< max).
        if let Some(vv) = self.cursor.last_dup()? {
            return Ok(Some((key, vv)));
        }
        Ok(None)
    }

    /// Returns a non-deleted latest version for exactly `key`, if any.
    fn seek_exact(&mut self, key: T::Key) -> BaseProofsStorageResult<Option<(T::Key, V)>> {
        if let Some((latest_key, latest_value)) = self.latest_version_for_key(key)?
            && let MaybeDeleted(Some(v)) = latest_value.value
        {
            return Ok(Some((latest_key, v)));
        }
        Ok(None)
    }

    /// Walk forward from `first_key` (inclusive) until we find a *live* latest-≤-max value.
    /// `first_key` must already be a *real key* in the table.
    fn next_live_from(
        &mut self,
        mut first_key: T::Key,
    ) -> BaseProofsStorageResult<Option<(T::Key, V)>> {
        loop {
            // Compute latest version ≤ max for this key
            if let Some((k, v)) = self.seek_exact(first_key.clone())? {
                return Ok(Some((k, v)));
            }

            // Move to next distinct key, or EOF
            let Some((next_key, _)) = self.cursor.next_no_dup()? else {
                return Ok(None);
            };

            first_key = next_key;
        }
    }

    /// Seek to the first non-deleted latest version at or after `start_key`.
    /// Logic:
    /// - Try exact key first (above). If alive, return it.
    /// - Otherwise hop to next distinct key and repeat until we find a live version or hit EOF.
    fn seek(&mut self, start_key: T::Key) -> BaseProofsStorageResult<Option<(T::Key, V)>> {
        // Position MDBX at first key >= start_key
        if let Some((first_key, _)) = self.cursor.seek(start_key)? {
            return self.next_live_from(first_key);
        }
        Ok(None)
    }

    /// Advance to the next distinct key from the current MDBX position
    /// and return its non-deleted latest version, if any.
    /// Next distinct key; if not positioned, start from `T::Key::default()`.
    fn next(&mut self) -> BaseProofsStorageResult<Option<(T::Key, V)>>
    where
        T::Key: Default,
    {
        // If not positioned, start from the beginning (default key).
        if self.cursor.current()?.is_none() {
            let Some((first_key, _)) = self.cursor.seek(T::Key::default())? else {
                return Ok(None);
            };
            return self.next_live_from(first_key);
        }

        // Otherwise advance to next distinct key and resume the walk.
        let Some((next_key, _)) = self.cursor.next_no_dup()? else {
            return Ok(None);
        };
        self.next_live_from(next_key)
    }
}

/// MDBX implementation of [`TrieCursor`].
#[derive(Debug)]
pub struct MdbxTrieCursor<T: Table + DupSort, Cursor> {
    inner: BlockNumberVersionedCursor<T, Cursor>,
    hashed_address: Option<B256>,
}

impl<
    V,
    T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
    Cursor: DbCursorRO<T> + DbDupCursorRO<T>,
> MdbxTrieCursor<T, Cursor>
{
    /// Initializes new [`MdbxTrieCursor`].
    pub const fn new(cursor: Cursor, max_block_number: u64, hashed_address: Option<B256>) -> Self {
        Self { inner: BlockNumberVersionedCursor::new(cursor, max_block_number), hashed_address }
    }
}

impl<Cursor> TrieCursor for MdbxTrieCursor<AccountTrieHistory, Cursor>
where
    Cursor: DbCursorRO<AccountTrieHistory> + DbDupCursorRO<AccountTrieHistory> + Send + Sync,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self
            .inner
            .seek_exact(StoredNibbles(path))
            .map(|opt| opt.map(|(StoredNibbles(n), node)| (n, node)))?)
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self
            .inner
            .seek(StoredNibbles(path))
            .map(|opt| opt.map(|(StoredNibbles(n), node)| (n, node)))?)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self.inner.next().map(|opt| opt.map(|(StoredNibbles(n), node)| (n, node)))?)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        self.inner.cursor.current().map(|opt| opt.map(|(StoredNibbles(n), _)| n))
    }

    fn reset(&mut self) {
        // Database cursors are stateless, no reset needed
    }
}

impl<Cursor> TrieCursor for MdbxTrieCursor<StorageTrieHistory, Cursor>
where
    Cursor: DbCursorRO<StorageTrieHistory> + DbDupCursorRO<StorageTrieHistory> + Send + Sync,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(address) = self.hashed_address {
            let key = StorageTrieKey::new(address, StoredNibbles(path));
            return Ok(self.inner.seek_exact(key).map(|opt| {
                opt.and_then(|(k, node)| (k.hashed_address == address).then_some((k.path.0, node)))
            })?);
        }
        Ok(None)
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(address) = self.hashed_address {
            let key = StorageTrieKey::new(address, StoredNibbles(path));
            return Ok(self.inner.seek(key).map(|opt| {
                opt.and_then(|(k, node)| (k.hashed_address == address).then_some((k.path.0, node)))
            })?);
        }
        Ok(None)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(address) = self.hashed_address {
            // If the cursor is not positioned, we need to seek to the first key for our bound
            // address to ensure we start iterating from the correct position in the
            // table. This is necessary because BlockNumberVersionedCursor::next() would
            // otherwise start from T::Key::default() (the beginning of the entire
            // table), which would cause us to miss entries for non-first addresses.
            if !self.inner.is_positioned()? {
                return self.seek(Nibbles::default());
            }

            return Ok(self.inner.next().map(|opt| {
                opt.and_then(|(k, node)| (k.hashed_address == address).then_some((k.path.0, node)))
            })?);
        }
        Ok(None)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        if let Some(address) = self.hashed_address {
            return self.inner.cursor.current().map(|opt| {
                opt.and_then(|(k, _)| (k.hashed_address == address).then_some(k.path.0))
            });
        }
        Ok(None)
    }

    fn reset(&mut self) {
        // Database cursors are stateless, no reset needed
    }
}

impl<Cursor> TrieStorageCursor for MdbxTrieCursor<StorageTrieHistory, Cursor>
where
    Cursor: DbCursorRO<StorageTrieHistory> + DbDupCursorRO<StorageTrieHistory> + Send + Sync,
{
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = Some(hashed_address);
    }
}

/// MDBX implementation of [`HashedCursor`] for storage state.
#[derive(Debug)]
pub struct MdbxStorageCursor<Cursor> {
    inner: BlockNumberVersionedCursor<HashedStorageHistory, Cursor>,
    hashed_address: B256,
}

impl<Cursor> MdbxStorageCursor<Cursor>
where
    Cursor: DbCursorRO<HashedStorageHistory> + DbDupCursorRO<HashedStorageHistory> + Send + Sync,
{
    ///  Initializes new [`MdbxStorageCursor`]
    pub const fn new(cursor: Cursor, block_number: u64, hashed_address: B256) -> Self {
        Self { inner: BlockNumberVersionedCursor::new(cursor, block_number), hashed_address }
    }
}

impl<Cursor> HashedCursor for MdbxStorageCursor<Cursor>
where
    Cursor: DbCursorRO<HashedStorageHistory> + DbDupCursorRO<HashedStorageHistory> + Send + Sync,
{
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let storage_key = HashedStorageKey::new(self.hashed_address, key);

        // hashed storage values can be zero, which means the storage slot is deleted, so we should
        // skip those
        let result = self.inner.seek(storage_key).map(|opt| {
            opt.and_then(|(k, v)| {
                // Only return entries that belong to the bound address
                (k.hashed_address == self.hashed_address).then_some((k.hashed_storage_key, v.0))
            })
        })?;

        if let Some((_, v)) = result
            && v.is_zero()
        {
            return self.next();
        }

        Ok(result)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        // If the cursor is not positioned, we need to seek to the first key for our bound address
        // to ensure we start iterating from the correct position in the table.
        // This is necessary because BlockNumberVersionedCursor::next() would otherwise start
        // from T::Key::default() (the beginning of the entire table), which would cause us
        // to miss entries for non-first addresses.
        if !self.inner.is_positioned()? {
            return self.seek(B256::ZERO);
        }

        loop {
            let result = self.inner.next().map(|opt| {
                opt.and_then(|(k, v)| {
                    // Only return entries that belong to the bound address
                    (k.hashed_address == self.hashed_address).then_some((k.hashed_storage_key, v.0))
                })
            })?;

            // hashed storage values can be zero, which means the storage slot is deleted, so we
            // should skip those
            if let Some((_, v)) = result
                && v.is_zero()
            {
                continue;
            }

            return Ok(result);
        }
    }

    fn reset(&mut self) {
        // Database cursors are stateless, no reset needed
    }
}

impl<Cursor> HashedStorageCursor for MdbxStorageCursor<Cursor>
where
    Cursor: DbCursorRO<HashedStorageHistory> + DbDupCursorRO<HashedStorageHistory> + Send + Sync,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address
    }
}

/// MDBX implementation of [`HashedCursor`] for account state.
#[derive(Debug)]
pub struct MdbxAccountCursor<Cursor> {
    inner: BlockNumberVersionedCursor<HashedAccountHistory, Cursor>,
}

impl<Cursor> MdbxAccountCursor<Cursor>
where
    Cursor: DbCursorRO<HashedAccountHistory> + DbDupCursorRO<HashedAccountHistory> + Send + Sync,
{
    /// Initializes new `MdbxAccountCursor`
    pub const fn new(cursor: Cursor, block_number: u64) -> Self {
        Self { inner: BlockNumberVersionedCursor::new(cursor, block_number) }
    }
}

impl<Cursor> HashedCursor for MdbxAccountCursor<Cursor>
where
    Cursor: DbCursorRO<HashedAccountHistory> + DbDupCursorRO<HashedAccountHistory> + Send + Sync,
{
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        Ok(self.inner.seek(key)?)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        Ok(self.inner.next()?)
    }

    fn reset(&mut self) {
        // Database cursors are stateless, no reset needed
    }
}

fn first_change_after(mut list: impl Iterator<Item = u64>, block_number: u64) -> Option<u64> {
    list.find(|changed_at| *changed_at > block_number)
}

/// V2 MDBX implementation of [`HashedCursor`] for account state.
#[derive(Debug)]
pub struct MdbxV2AccountCursor<Current, History, Changeset> {
    current: Current,
    history: History,
    changeset: Changeset,
    max_block_number: u64,
    position: Option<B256>,
}

impl<Current, History, Changeset> MdbxV2AccountCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2HashedAccounts>,
    History: DbCursorRO<V2HashedAccountsHistory>,
    Changeset: DbCursorRO<V2HashedAccountChangeSets>
        + DbDupCursorRO<V2HashedAccountChangeSets>,
{
    /// Builds a direct V2 account cursor at `max_block_number`.
    pub fn new(tx: &impl DbTx<Cursor<V2HashedAccounts> = Current, Cursor<V2HashedAccountsHistory> = History, DupCursor<V2HashedAccountChangeSets> = Changeset>, max_block_number: u64) -> BaseProofsStorageResult<Self> {
        Ok(Self {
            current: tx.cursor_read::<V2HashedAccounts>()?,
            history: tx.cursor_read::<V2HashedAccountsHistory>()?,
            changeset: tx.cursor_dup_read::<V2HashedAccountChangeSets>()?,
            max_block_number,
            position: None,
        })
    }

    fn first_future_change(&mut self, key: B256) -> BaseProofsStorageResult<Option<u64>> {
        let mut row = self.history.seek(HashedAccountShardedKey::new(key, 0))?;
        while let Some((history_key, list)) = row {
            if history_key.0.key != key {
                break;
            }
            if let Some(changed_at) = first_change_after(list.iter(), self.max_block_number) {
                return Ok(Some(changed_at));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn resolve_key(&mut self, key: B256) -> BaseProofsStorageResult<Option<Account>> {
        if let Some(changed_at) = self.first_future_change(key)? {
            return Ok(self
                .changeset
                .seek_by_key_subkey(changed_at, key)?
                .filter(|entry| entry.hashed_address == key)
                .and_then(|entry| entry.info));
        }

        Ok(self.current.seek_exact(key)?.map(|(_, account)| account))
    }

    fn next_current_at_or_after(&mut self, key: B256) -> BaseProofsStorageResult<Option<B256>> {
        Ok(self.current.seek(key)?.map(|(row_key, _)| row_key))
    }

    fn next_current_after(&mut self, key: B256) -> BaseProofsStorageResult<Option<B256>> {
        let row = self.current.seek(key)?;
        match row {
            Some((row_key, _)) if row_key == key => Ok(self.current.next()?.map(|(next_key, _)| next_key)),
            Some((row_key, _)) => Ok(Some(row_key)),
            None => Ok(None),
        }
    }

    fn next_history_at_or_after(&mut self, key: B256) -> BaseProofsStorageResult<Option<B256>> {
        Ok(self
            .history
            .seek(HashedAccountShardedKey::new(key, 0))?
            .map(|(history_key, _)| history_key.0.key))
    }

    fn next_history_after(&mut self, key: B256) -> BaseProofsStorageResult<Option<B256>> {
        let mut row = self.history.seek(HashedAccountShardedKey::new(key, u64::MAX))?;
        while let Some((history_key, _)) = row {
            if history_key.0.key > key {
                return Ok(Some(history_key.0.key));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn seek_from_candidates(
        &mut self,
        mut current_key: Option<B256>,
        mut history_key: Option<B256>,
    ) -> BaseProofsStorageResult<Option<(B256, Account)>> {
        loop {
            let candidate = match (current_key, history_key) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            };

            let Some(candidate) = candidate else {
                self.position = None;
                return Ok(None);
            };

            if let Some(account) = self.resolve_key(candidate)? {
                self.position = Some(candidate);
                return Ok(Some((candidate, account)));
            }

            current_key = self.next_current_after(candidate)?;
            history_key = self.next_history_after(candidate)?;
        }
    }
}

impl<Current, History, Changeset> HashedCursor for MdbxV2AccountCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2HashedAccounts> + Send + Sync,
    History: DbCursorRO<V2HashedAccountsHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedAccountChangeSets>
        + DbDupCursorRO<V2HashedAccountChangeSets>
        + Send
        + Sync,
{
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let current_key = self.next_current_at_or_after(key)?;
        let history_key = self.next_history_at_or_after(key)?;
        Ok(self.seek_from_candidates(current_key, history_key)?)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if let Some(position) = self.position {
            let current_key = self.next_current_after(position)?;
            let history_key = self.next_history_after(position)?;
            return Ok(self.seek_from_candidates(current_key, history_key)?);
        }

        self.seek(B256::ZERO)
    }

    fn reset(&mut self) {
        self.position = None;
    }
}

/// V2 account cursor over current state, used when the requested block is the latest proof block.
#[derive(Debug)]
pub struct MdbxV2LatestAccountCursor<Cursor> {
    cursor: Cursor,
    positioned: bool,
}

impl<Cursor> MdbxV2LatestAccountCursor<Cursor> {
    /// Creates a latest-state account cursor backed directly by MDBX.
    pub const fn new(cursor: Cursor) -> Self {
        Self { cursor, positioned: false }
    }
}

impl<Cursor> HashedCursor for MdbxV2LatestAccountCursor<Cursor>
where
    Cursor: DbCursorRO<V2HashedAccounts> + Send + Sync,
{
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.positioned = true;
        self.cursor.seek(key)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.positioned {
            return self.seek(B256::ZERO);
        }
        self.cursor.next()
    }

    fn reset(&mut self) {
        self.positioned = false;
    }
}

/// V2 account cursor that uses direct current-state iteration when possible.
#[derive(Debug)]
pub enum MdbxV2AccountCursorEither<Cursor, History, Changeset> {
    /// Latest proof block, backed by the current-state table cursor.
    Latest(MdbxV2LatestAccountCursor<Cursor>),
    /// Historical block, backed by direct current/history index reads.
    Historical(MdbxV2AccountCursor<Cursor, History, Changeset>),
}

impl<Cursor, History, Changeset> HashedCursor for MdbxV2AccountCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2HashedAccounts> + Send + Sync,
    History: DbCursorRO<V2HashedAccountsHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedAccountChangeSets>
        + DbDupCursorRO<V2HashedAccountChangeSets>
        + Send
        + Sync,
{
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek(key),
            Self::Historical(cursor) => cursor.seek(key),
        }
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.next(),
            Self::Historical(cursor) => cursor.next(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Latest(cursor) => cursor.reset(),
            Self::Historical(cursor) => cursor.reset(),
        }
    }
}

/// V2 MDBX implementation of [`TrieCursor`] for account trie nodes.
#[derive(Debug)]
pub struct MdbxV2AccountTrieCursor<Current, History, Changeset> {
    current: Current,
    history: History,
    changeset: Changeset,
    max_block_number: u64,
    position: Option<StoredNibbles>,
}

impl<Current, History, Changeset> MdbxV2AccountTrieCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2AccountsTrie>,
    History: DbCursorRO<V2AccountsTrieHistory>,
    Changeset: DbCursorRO<V2AccountTrieChangeSets>
        + DbDupCursorRO<V2AccountTrieChangeSets>,
{
    /// Builds a direct V2 account trie cursor at `max_block_number`.
    pub fn new(tx: &impl DbTx<Cursor<V2AccountsTrie> = Current, Cursor<V2AccountsTrieHistory> = History, DupCursor<V2AccountTrieChangeSets> = Changeset>, max_block_number: u64) -> BaseProofsStorageResult<Self> {
        Ok(Self {
            current: tx.cursor_read::<V2AccountsTrie>()?,
            history: tx.cursor_read::<V2AccountsTrieHistory>()?,
            changeset: tx.cursor_dup_read::<V2AccountTrieChangeSets>()?,
            max_block_number,
            position: None,
        })
    }

    fn first_future_change(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<u64>> {
        let mut row = self.history.seek(AccountTrieShardedKey::new(key.clone(), 0))?;
        while let Some((history_key, list)) = row {
            if history_key.key != *key {
                break;
            }
            if let Some(changed_at) = first_change_after(list.iter(), self.max_block_number) {
                return Ok(Some(changed_at));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn resolve_key(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<BranchNodeCompact>> {
        if let Some(changed_at) = self.first_future_change(key)? {
            let subkey = StoredNibblesSubKey::from(key.0.clone());
            return Ok(self
                .changeset
                .seek_by_key_subkey(changed_at, subkey.clone())?
                .filter(|entry| entry.nibbles == subkey)
                .and_then(|entry| entry.node));
        }

        Ok(self.current.seek_exact(key.clone())?.map(|(_, node)| node))
    }

    fn next_current_at_or_after(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        Ok(self.current.seek(key.clone())?.map(|(row_key, _)| row_key))
    }

    fn next_current_after(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        let row = self.current.seek(key.clone())?;
        match row {
            Some((row_key, _)) if row_key == *key => Ok(self.current.next()?.map(|(next_key, _)| next_key)),
            Some((row_key, _)) => Ok(Some(row_key)),
            None => Ok(None),
        }
    }

    fn next_history_at_or_after(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        Ok(self
            .history
            .seek(AccountTrieShardedKey::new(key.clone(), 0))?
            .map(|(history_key, _)| history_key.key))
    }

    fn next_history_after(
        &mut self,
        key: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        let mut row = self.history.seek(AccountTrieShardedKey::new(key.clone(), u64::MAX))?;
        while let Some((history_key, _)) = row {
            if history_key.key > *key {
                return Ok(Some(history_key.key));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn seek_from_candidates(
        &mut self,
        mut current_key: Option<StoredNibbles>,
        mut history_key: Option<StoredNibbles>,
    ) -> BaseProofsStorageResult<Option<(StoredNibbles, BranchNodeCompact)>> {
        loop {
            let candidate = match (current_key.clone(), history_key.clone()) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            };

            let Some(candidate) = candidate else {
                self.position = None;
                return Ok(None);
            };

            if let Some(node) = self.resolve_key(&candidate)? {
                self.position = Some(candidate.clone());
                return Ok(Some((candidate, node)));
            }

            current_key = self.next_current_after(&candidate)?;
            history_key = self.next_history_after(&candidate)?;
        }
    }
}

impl<Current, History, Changeset> TrieCursor
    for MdbxV2AccountTrieCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2AccountsTrie> + Send + Sync,
    History: DbCursorRO<V2AccountsTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2AccountTrieChangeSets>
        + DbDupCursorRO<V2AccountTrieChangeSets>
        + Send
        + Sync,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let key = StoredNibbles(path);
        if let Some(node) = self.resolve_key(&key)? {
            self.position = Some(key.clone());
            return Ok(Some((key.0, node)));
        }
        self.position = None;
        Ok(None)
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let current_key = self.next_current_at_or_after(&StoredNibbles(path.clone()))?;
        let history_key = self.next_history_at_or_after(&StoredNibbles(path))?;
        Ok(self.seek_from_candidates(current_key, history_key)?.map(|(key, node)| (key.0, node)))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(position) = self.position.clone() {
            let current_key = self.next_current_after(&position)?;
            let history_key = self.next_history_after(&position)?;
            return Ok(self.seek_from_candidates(current_key, history_key)?.map(|(key, node)| (key.0, node)));
        }

        self.seek(Nibbles::default())
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.position.as_ref().map(|key| key.0.clone()))
    }

    fn reset(&mut self) {
        self.position = None;
    }
}

/// V2 account-trie cursor over current state, used for latest proof-state root calculation.
#[derive(Debug)]
pub struct MdbxV2LatestAccountTrieCursor<Cursor> {
    cursor: Cursor,
    positioned: bool,
}

impl<Cursor> MdbxV2LatestAccountTrieCursor<Cursor> {
    /// Creates a latest-state account-trie cursor backed directly by MDBX.
    pub const fn new(cursor: Cursor) -> Self {
        Self { cursor, positioned: false }
    }
}

impl<Cursor> TrieCursor for MdbxV2LatestAccountTrieCursor<Cursor>
where
    Cursor: DbCursorRO<V2AccountsTrie>,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.positioned = true;
        Ok(self.cursor.seek_exact(StoredNibbles(path))?.map(|(key, node)| (key.0, node)))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.positioned = true;
        Ok(self.cursor.seek(StoredNibbles(path))?.map(|(key, node)| (key.0, node)))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if !self.positioned {
            return self.seek(Nibbles::default());
        }
        Ok(self.cursor.next()?.map(|(key, node)| (key.0, node)))
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        if !self.positioned {
            return Ok(None);
        }
        Ok(self.cursor.current()?.map(|(key, _)| key.0))
    }

    fn reset(&mut self) {
        self.positioned = false;
    }
}

/// V2 account-trie cursor that uses direct current-state iteration when possible.
#[derive(Debug)]
pub enum MdbxV2AccountTrieCursorEither<Cursor, History, Changeset> {
    /// Latest proof block, backed by the current-state table cursor.
    Latest(MdbxV2LatestAccountTrieCursor<Cursor>),
    /// Historical block, backed by direct current/history index reads.
    Historical(MdbxV2AccountTrieCursor<Cursor, History, Changeset>),
}

impl<Cursor, History, Changeset> TrieCursor
    for MdbxV2AccountTrieCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2AccountsTrie> + Send + Sync,
    History: DbCursorRO<V2AccountsTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2AccountTrieChangeSets>
        + DbDupCursorRO<V2AccountTrieChangeSets>
        + Send
        + Sync,
{
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek_exact(key),
            Self::Historical(cursor) => cursor.seek_exact(key),
        }
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek(key),
            Self::Historical(cursor) => cursor.seek(key),
        }
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.next(),
            Self::Historical(cursor) => cursor.next(),
        }
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.current(),
            Self::Historical(cursor) => cursor.current(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Latest(cursor) => cursor.reset(),
            Self::Historical(cursor) => cursor.reset(),
        }
    }
}

/// V2 MDBX implementation of [`HashedCursor`] for storage state.
#[derive(Debug)]
pub struct MdbxV2StorageCursor<Current, History, Changeset> {
    current: Current,
    history: History,
    changeset: Changeset,
    max_block_number: u64,
    hashed_address: B256,
    position: Option<B256>,
}

impl<Current, History, Changeset> MdbxV2StorageCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages>,
    History: DbCursorRO<V2HashedStoragesHistory>,
    Changeset: DbCursorRO<V2HashedStorageChangeSets>
        + DbDupCursorRO<V2HashedStorageChangeSets>,
{
    /// Builds a direct V2 storage cursor at `max_block_number`.
    pub fn new(
        tx: &impl DbTx<Cursor<V2HashedStoragesHistory> = History, DupCursor<V2HashedStorages> = Current, DupCursor<V2HashedStorageChangeSets> = Changeset>,
        max_block_number: u64,
        hashed_address: B256,
    ) -> BaseProofsStorageResult<Self> {
        Ok(Self {
            current: tx.cursor_dup_read::<V2HashedStorages>()?,
            history: tx.cursor_read::<V2HashedStoragesHistory>()?,
            changeset: tx.cursor_dup_read::<V2HashedStorageChangeSets>()?,
            max_block_number,
            hashed_address,
            position: None,
        })
    }

    fn first_future_change(&mut self, slot: B256) -> BaseProofsStorageResult<Option<u64>> {
        let mut row =
            self.history.seek(HashedStorageShardedKey::new(self.hashed_address, slot, 0))?;
        while let Some((history_key, list)) = row {
            if history_key.hashed_address != self.hashed_address || history_key.sharded_key.key != slot {
                break;
            }
            if let Some(changed_at) = first_change_after(list.iter(), self.max_block_number) {
                return Ok(Some(changed_at));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn current_entry_at_or_after(
        &mut self,
        slot: B256,
    ) -> BaseProofsStorageResult<Option<StorageEntry>> {
        let entry = self.current.seek_by_key_subkey(self.hashed_address, slot)?;
        let current_address = self.current.current()?.map(|(address, _)| address);
        Ok((current_address == Some(self.hashed_address)).then_some(entry).flatten())
    }

    fn next_current_at_or_after(&mut self, slot: B256) -> BaseProofsStorageResult<Option<B256>> {
        Ok(self.current_entry_at_or_after(slot)?.map(|entry| entry.key))
    }

    fn next_current_after(&mut self, slot: B256) -> BaseProofsStorageResult<Option<B256>> {
        let Some(entry) = self.current_entry_at_or_after(slot)? else {
            return Ok(None);
        };
        if entry.key > slot {
            return Ok(Some(entry.key));
        }

        loop {
            let next = self.current.next_dup()?;
            let current_address = next.as_ref().map(|(address, _)| *address);
            if current_address != Some(self.hashed_address) {
                return Ok(None);
            }
            let Some((_, entry)) = next else {
                return Ok(None);
            };
            if entry.key > slot {
                return Ok(Some(entry.key));
            }
        }
    }

    fn next_history_at_or_after(&mut self, slot: B256) -> BaseProofsStorageResult<Option<B256>> {
        Ok(self
            .history
            .seek(HashedStorageShardedKey::new(self.hashed_address, slot, 0))?
            .and_then(|(history_key, _)| {
                (history_key.hashed_address == self.hashed_address)
                    .then_some(history_key.sharded_key.key)
            }))
    }

    fn next_history_after(&mut self, slot: B256) -> BaseProofsStorageResult<Option<B256>> {
        let mut row = self
            .history
            .seek(HashedStorageShardedKey::new(self.hashed_address, slot, u64::MAX))?;
        while let Some((history_key, _)) = row {
            if history_key.hashed_address != self.hashed_address {
                break;
            }
            if history_key.sharded_key.key > slot {
                return Ok(Some(history_key.sharded_key.key));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn resolve_key(&mut self, slot: B256) -> BaseProofsStorageResult<Option<U256>> {
        if let Some(changed_at) = self.first_future_change(slot)? {
            return Ok(self
                .changeset
                .seek_by_key_subkey(BlockNumberHashedAddress((changed_at, self.hashed_address)), slot)?
                .filter(|entry| entry.key == slot)
                .and_then(|entry| (!entry.value.is_zero()).then_some(entry.value)));
        }

        Ok(self
            .current_entry_at_or_after(slot)?
            .filter(|entry| entry.key == slot && !entry.value.is_zero())
            .map(|entry| entry.value))
    }

    fn seek_from_candidates(
        &mut self,
        mut current_key: Option<B256>,
        mut history_key: Option<B256>,
    ) -> BaseProofsStorageResult<Option<(B256, U256)>> {
        loop {
            let candidate = match (current_key, history_key) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            };

            let Some(candidate) = candidate else {
                self.position = None;
                return Ok(None);
            };

            if let Some(value) = self.resolve_key(candidate)? {
                self.position = Some(candidate);
                return Ok(Some((candidate, value)));
            }

            current_key = self.next_current_after(candidate)?;
            history_key = self.next_history_after(candidate)?;
        }
    }
}

impl<Current, History, Changeset> HashedCursor for MdbxV2StorageCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
    History: DbCursorRO<V2HashedStoragesHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedStorageChangeSets>
        + DbDupCursorRO<V2HashedStorageChangeSets>
        + Send
        + Sync,
{
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let current_key = self.next_current_at_or_after(key)?;
        let history_key = self.next_history_at_or_after(key)?;
        Ok(self.seek_from_candidates(current_key, history_key)?)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if let Some(position) = self.position {
            let current_key = self.next_current_after(position)?;
            let history_key = self.next_history_after(position)?;
            return Ok(self.seek_from_candidates(current_key, history_key)?);
        }

        self.seek(B256::ZERO)
    }

    fn reset(&mut self) {
        self.position = None;
    }
}

impl<Current, History, Changeset> HashedStorageCursor
    for MdbxV2StorageCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
    History: DbCursorRO<V2HashedStoragesHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedStorageChangeSets>
        + DbDupCursorRO<V2HashedStorageChangeSets>
        + Send
        + Sync,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.position = None;
    }
}

/// V2 storage cursor over current state for one account.
#[derive(Debug)]
pub struct MdbxV2LatestStorageCursor<Cursor> {
    cursor: Cursor,
    hashed_address: B256,
    positioned: bool,
    last_key: Option<B256>,
}

impl<Cursor> MdbxV2LatestStorageCursor<Cursor> {
    /// Creates a latest-state storage cursor backed directly by the dup-sort MDBX table.
    pub const fn new(cursor: Cursor, hashed_address: B256) -> Self {
        Self { cursor, hashed_address, positioned: false, last_key: None }
    }
}

impl<Cursor> HashedCursor for MdbxV2LatestStorageCursor<Cursor>
where
    Cursor: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
{
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.positioned = true;
        self.last_key = None;
        let entry = self.cursor.seek_by_key_subkey(self.hashed_address, key)?;
        let current_address = self.cursor.current()?.map(|(address, _)| address);
        self.live_storage_entry(current_address, entry)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.positioned {
            return self.seek(B256::ZERO);
        }
        loop {
            let entry = self.cursor.next_dup()?;
            let current_address = entry.as_ref().map(|(address, _)| *address);
            let Some(entry) = entry.map(|(_, entry)| entry) else {
                return Ok(None);
            };
            if self.last_key.is_some_and(|last_key| entry.key == last_key) {
                continue;
            }
            return self.live_storage_entry(current_address, Some(entry));
        }
    }

    fn reset(&mut self) {
        self.positioned = false;
        self.last_key = None;
    }
}

impl<Cursor> MdbxV2LatestStorageCursor<Cursor>
where
    Cursor: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
{
    fn live_storage_entry(
        &mut self,
        current_address: Option<B256>,
        entry: Option<StorageEntry>,
    ) -> Result<Option<(B256, U256)>, DatabaseError> {
        if current_address != Some(self.hashed_address) {
            return Ok(None);
        }
        let Some(entry) = entry else {
            return Ok(None);
        };
        if !entry.value.is_zero() {
            self.last_key = Some(entry.key);
            return Ok(Some((entry.key, entry.value)));
        }

        self.last_key = Some(entry.key);
        loop {
            let entry = self.cursor.next_dup()?;
            let current_address = entry.as_ref().map(|(address, _)| *address);
            if current_address != Some(self.hashed_address) {
                return Ok(None);
            }
            let Some((_, entry)) = entry else {
                return Ok(None);
            };
            if self.last_key.is_some_and(|last_key| entry.key == last_key) {
                continue;
            }
            if entry.value.is_zero() {
                self.last_key = Some(entry.key);
                continue;
            }
            self.last_key = Some(entry.key);
            return Ok(Some((entry.key, entry.value)));
        }
    }
}

impl<Cursor> HashedStorageCursor for MdbxV2LatestStorageCursor<Cursor>
where
    Cursor: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.positioned = false;
        self.last_key = None;
    }
}

/// V2 storage cursor that uses direct current-state iteration when possible.
#[derive(Debug)]
pub enum MdbxV2StorageCursorEither<Cursor, History, Changeset> {
    /// Latest proof block, backed by the current-state table cursor.
    Latest(MdbxV2LatestStorageCursor<Cursor>),
    /// Historical block, backed by direct current/history index reads.
    Historical(MdbxV2StorageCursor<Cursor, History, Changeset>),
}

impl<Cursor, History, Changeset> HashedCursor
    for MdbxV2StorageCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
    History: DbCursorRO<V2HashedStoragesHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedStorageChangeSets>
        + DbDupCursorRO<V2HashedStorageChangeSets>
        + Send
        + Sync,
{
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek(key),
            Self::Historical(cursor) => cursor.seek(key),
        }
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.next(),
            Self::Historical(cursor) => cursor.next(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Latest(cursor) => cursor.reset(),
            Self::Historical(cursor) => cursor.reset(),
        }
    }
}

impl<Cursor, History, Changeset> HashedStorageCursor
    for MdbxV2StorageCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2HashedStorages> + DbDupCursorRO<V2HashedStorages> + Send + Sync,
    History: DbCursorRO<V2HashedStoragesHistory> + Send + Sync,
    Changeset: DbCursorRO<V2HashedStorageChangeSets>
        + DbDupCursorRO<V2HashedStorageChangeSets>
        + Send
        + Sync,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.is_storage_empty(),
            Self::Historical(cursor) => cursor.is_storage_empty(),
        }
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        match self {
            Self::Latest(cursor) => cursor.set_hashed_address(hashed_address),
            Self::Historical(cursor) => cursor.set_hashed_address(hashed_address),
        }
    }
}

/// V2 MDBX implementation of [`TrieCursor`] for storage trie nodes.
#[derive(Debug)]
pub struct MdbxV2StorageTrieCursor<Current, History, Changeset> {
    current: Current,
    history: History,
    changeset: Changeset,
    max_block_number: u64,
    hashed_address: B256,
    position: Option<StoredNibbles>,
}

impl<Current, History, Changeset> MdbxV2StorageTrieCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie>,
    History: DbCursorRO<V2StoragesTrieHistory>,
    Changeset: DbCursorRO<V2StorageTrieChangeSets> + DbDupCursorRO<V2StorageTrieChangeSets>,
{
    /// Builds a direct V2 storage trie cursor at `max_block_number`.
    pub fn new(
        tx: &impl DbTx<Cursor<V2StoragesTrieHistory> = History, DupCursor<V2StoragesTrie> = Current, DupCursor<V2StorageTrieChangeSets> = Changeset>,
        max_block_number: u64,
        hashed_address: B256,
    ) -> BaseProofsStorageResult<Self> {
        Ok(Self {
            current: tx.cursor_dup_read::<V2StoragesTrie>()?,
            history: tx.cursor_read::<V2StoragesTrieHistory>()?,
            changeset: tx.cursor_dup_read::<V2StorageTrieChangeSets>()?,
            max_block_number,
            hashed_address,
            position: None,
        })
    }

    fn first_future_change(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<u64>> {
        let mut row =
            self.history.seek(StorageTrieShardedKey::new(self.hashed_address, path.clone(), 0))?;
        while let Some((history_key, list)) = row {
            if history_key.hashed_address != self.hashed_address || history_key.key != *path {
                break;
            }
            if let Some(changed_at) = first_change_after(list.iter(), self.max_block_number) {
                return Ok(Some(changed_at));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn current_entry_at_or_after(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<(StoredNibblesSubKey, BranchNodeCompact)>> {
        let subkey = StoredNibblesSubKey::from(path.0.clone());
        let entry = self.current.seek_by_key_subkey(self.hashed_address, subkey)?;
        let current_address = self.current.current()?.map(|(address, _)| address);
        Ok((current_address == Some(self.hashed_address))
            .then_some(entry.map(|entry| (entry.nibbles, entry.node)))
            .flatten())
    }

    fn next_current_at_or_after(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        Ok(self.current_entry_at_or_after(path)?.map(|(nibbles, _)| StoredNibbles(nibbles.0)))
    }

    fn next_current_after(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        let Some((nibbles, _)) = self.current_entry_at_or_after(path)? else {
            return Ok(None);
        };
        if nibbles.0 > path.0 {
            return Ok(Some(StoredNibbles(nibbles.0)));
        }

        loop {
            let next = self.current.next_dup()?;
            let current_address = next.as_ref().map(|(address, _)| *address);
            if current_address != Some(self.hashed_address) {
                return Ok(None);
            }
            let Some((_, entry)) = next else {
                return Ok(None);
            };
            if entry.nibbles.0 > path.0 {
                return Ok(Some(StoredNibbles(entry.nibbles.0)));
            }
        }
    }

    fn next_history_at_or_after(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        Ok(self
            .history
            .seek(StorageTrieShardedKey::new(self.hashed_address, path.clone(), 0))?
            .and_then(|(history_key, _)| {
                (history_key.hashed_address == self.hashed_address).then_some(history_key.key)
            }))
    }

    fn next_history_after(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<StoredNibbles>> {
        let mut row = self
            .history
            .seek(StorageTrieShardedKey::new(self.hashed_address, path.clone(), u64::MAX))?;
        while let Some((history_key, _)) = row {
            if history_key.hashed_address != self.hashed_address {
                break;
            }
            if history_key.key > *path {
                return Ok(Some(history_key.key));
            }
            row = self.history.next()?;
        }
        Ok(None)
    }

    fn resolve_key(
        &mut self,
        path: &StoredNibbles,
    ) -> BaseProofsStorageResult<Option<BranchNodeCompact>> {
        if let Some(changed_at) = self.first_future_change(path)? {
            let subkey = StoredNibblesSubKey::from(path.0.clone());
            return Ok(self
                .changeset
                .seek_by_key_subkey(BlockNumberHashedAddress((changed_at, self.hashed_address)), subkey.clone())?
                .filter(|entry| entry.nibbles == subkey)
                .and_then(|entry| entry.node));
        }

        Ok(self
            .current_entry_at_or_after(path)?
            .filter(|(nibbles, _)| nibbles.0 == path.0)
            .map(|(_, node)| node))
    }

    fn seek_from_candidates(
        &mut self,
        mut current_key: Option<StoredNibbles>,
        mut history_key: Option<StoredNibbles>,
    ) -> BaseProofsStorageResult<Option<(StoredNibbles, BranchNodeCompact)>> {
        loop {
            let candidate = match (current_key.clone(), history_key.clone()) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            };

            let Some(candidate) = candidate else {
                self.position = None;
                return Ok(None);
            };

            if let Some(node) = self.resolve_key(&candidate)? {
                self.position = Some(candidate.clone());
                return Ok(Some((candidate, node)));
            }

            current_key = self.next_current_after(&candidate)?;
            history_key = self.next_history_after(&candidate)?;
        }
    }
}

impl<Current, History, Changeset> TrieCursor for MdbxV2StorageTrieCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie> + Send + Sync,
    History: DbCursorRO<V2StoragesTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2StorageTrieChangeSets>
        + DbDupCursorRO<V2StorageTrieChangeSets>
        + Send
        + Sync,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let key = StoredNibbles(path);
        if let Some(node) = self.resolve_key(&key)? {
            self.position = Some(key.clone());
            return Ok(Some((key.0, node)));
        }
        self.position = None;
        Ok(None)
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let current_key = self.next_current_at_or_after(&StoredNibbles(path.clone()))?;
        let history_key = self.next_history_at_or_after(&StoredNibbles(path))?;
        Ok(self.seek_from_candidates(current_key, history_key)?.map(|(key, node)| (key.0, node)))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if let Some(position) = self.position.clone() {
            let current_key = self.next_current_after(&position)?;
            let history_key = self.next_history_after(&position)?;
            return Ok(self.seek_from_candidates(current_key, history_key)?.map(|(key, node)| (key.0, node)));
        }

        self.seek(Nibbles::default())
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.position.as_ref().map(|key| key.0.clone()))
    }

    fn reset(&mut self) {
        self.position = None;
    }
}

impl<Current, History, Changeset> TrieStorageCursor
    for MdbxV2StorageTrieCursor<Current, History, Changeset>
where
    Current: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie> + Send + Sync,
    History: DbCursorRO<V2StoragesTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2StorageTrieChangeSets>
        + DbDupCursorRO<V2StorageTrieChangeSets>
        + Send
        + Sync,
{
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.position = None;
    }
}

/// V2 storage-trie cursor over current state for one account.
#[derive(Debug)]
pub struct MdbxV2LatestStorageTrieCursor<Cursor> {
    cursor: Cursor,
    hashed_address: B256,
    positioned: bool,
    last_path: Option<StoredNibblesSubKey>,
}

impl<Cursor> MdbxV2LatestStorageTrieCursor<Cursor> {
    /// Creates a latest-state storage-trie cursor backed directly by the dup-sort MDBX table.
    pub const fn new(cursor: Cursor, hashed_address: B256) -> Self {
        Self { cursor, hashed_address, positioned: false, last_path: None }
    }
}

impl<Cursor> TrieCursor for MdbxV2LatestStorageTrieCursor<Cursor>
where
    Cursor: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie>,
{
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.positioned = true;
        self.last_path = None;
        let subkey = StoredNibblesSubKey::from(path);
        let entry = self.cursor.seek_by_key_subkey(self.hashed_address, subkey.clone())?;
        let current_address = self.cursor.current()?.map(|(address, _)| address);
        let out = entry.and_then(|entry| {
            (current_address == Some(self.hashed_address) && entry.nibbles == subkey)
                .then_some((entry.nibbles, entry.node))
        });
        Ok(out.map(|(nibbles, node)| {
            self.last_path = Some(nibbles);
            (path, node)
        }))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.positioned = true;
        self.last_path = None;
        let entry =
            self.cursor.seek_by_key_subkey(self.hashed_address, StoredNibblesSubKey::from(path))?;
        let current_address = self.cursor.current()?.map(|(address, _)| address);
        let out = entry.and_then(|entry| {
            (current_address == Some(self.hashed_address)).then_some((entry.nibbles, entry.node))
        });
        Ok(out.map(|(nibbles, node)| {
            self.last_path = Some(nibbles.clone());
            (nibbles.0, node)
        }))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if !self.positioned {
            return self.seek(Nibbles::default());
        }
        loop {
            let Some((address, entry)) = self.cursor.next_dup()? else {
                return Ok(None);
            };
            if address != self.hashed_address {
                return Ok(None);
            }
            if self.last_path.as_ref().is_some_and(|last_path| entry.nibbles == *last_path) {
                continue;
            }
            self.last_path = Some(entry.nibbles.clone());
            return Ok(Some((entry.nibbles.0, entry.node)));
        }
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.cursor.current()?.and_then(|(address, entry)| {
            (address == self.hashed_address).then_some(entry.nibbles.0)
        }))
    }

    fn reset(&mut self) {
        self.positioned = false;
        self.last_path = None;
    }
}

impl<Cursor> TrieStorageCursor for MdbxV2LatestStorageTrieCursor<Cursor>
where
    Cursor: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie>,
{
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.positioned = false;
        self.last_path = None;
    }
}

/// V2 storage-trie cursor that uses direct current-state iteration when possible.
#[derive(Debug)]
pub enum MdbxV2StorageTrieCursorEither<Cursor, History, Changeset> {
    /// Latest proof block, backed by the current-state table cursor.
    Latest(MdbxV2LatestStorageTrieCursor<Cursor>),
    /// Historical block, backed by direct current/history index reads.
    Historical(MdbxV2StorageTrieCursor<Cursor, History, Changeset>),
}

impl<Cursor, History, Changeset> TrieCursor
    for MdbxV2StorageTrieCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie> + Send + Sync,
    History: DbCursorRO<V2StoragesTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2StorageTrieChangeSets>
        + DbDupCursorRO<V2StorageTrieChangeSets>
        + Send
        + Sync,
{
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek_exact(key),
            Self::Historical(cursor) => cursor.seek_exact(key),
        }
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.seek(key),
            Self::Historical(cursor) => cursor.seek(key),
        }
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.next(),
            Self::Historical(cursor) => cursor.next(),
        }
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        match self {
            Self::Latest(cursor) => cursor.current(),
            Self::Historical(cursor) => cursor.current(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Latest(cursor) => cursor.reset(),
            Self::Historical(cursor) => cursor.reset(),
        }
    }
}

impl<Cursor, History, Changeset> TrieStorageCursor
    for MdbxV2StorageTrieCursorEither<Cursor, History, Changeset>
where
    Cursor: DbCursorRO<V2StoragesTrie> + DbDupCursorRO<V2StoragesTrie> + Send + Sync,
    History: DbCursorRO<V2StoragesTrieHistory> + Send + Sync,
    Changeset: DbCursorRO<V2StorageTrieChangeSets>
        + DbDupCursorRO<V2StorageTrieChangeSets>
        + Send
        + Sync,
{
    fn set_hashed_address(&mut self, hashed_address: B256) {
        match self {
            Self::Latest(cursor) => cursor.set_hashed_address(hashed_address),
            Self::Historical(cursor) => cursor.set_hashed_address(hashed_address),
        }
    }
}

#[cfg(test)]
mod tests {
    use reth_db::{
        DatabaseEnv,
        mdbx::{DatabaseArguments, init_db_for},
    };
    use reth_db_api::{
        Database,
        cursor::{DbCursorRW, DbDupCursorRW},
        transaction::{DbTx, DbTxMut},
    };
    use reth_trie::{BranchNodeCompact, Nibbles, StoredNibbles};
    use reth_trie_common::{StorageTrieEntry, StoredNibblesSubKey};
    use tempfile::TempDir;

    use super::*;
    use crate::db::{HashedAccountBeforeTx, StorageValue, TrieChangeSetsEntry, models};

    fn setup_db() -> DatabaseEnv {
        let tmp = TempDir::new().expect("create tmpdir");
        init_db_for::<_, models::Tables>(tmp, DatabaseArguments::default()).expect("init db")
    }

    fn stored(path: Nibbles) -> StoredNibbles {
        StoredNibbles(path)
    }

    fn node() -> BranchNodeCompact {
        BranchNodeCompact::default()
    }

    fn append_account_trie(
        wtx: &<DatabaseEnv as Database>::TXMut,
        key: StoredNibbles,
        block: u64,
        val: Option<BranchNodeCompact>,
    ) {
        let mut c = wtx.cursor_dup_write::<AccountTrieHistory>().expect("dup write cursor");
        let vv = VersionedValue { block_number: block, value: MaybeDeleted(val) };
        c.append_dup(key, vv).expect("append dup");
    }

    fn append_storage_trie(
        wtx: &<DatabaseEnv as Database>::TXMut,
        address: B256,
        path: Nibbles,
        block: u64,
        val: Option<BranchNodeCompact>,
    ) {
        let mut c = wtx.cursor_dup_write::<StorageTrieHistory>().expect("dup write cursor");
        let key = StorageTrieKey::new(address, StoredNibbles(path));
        let vv = VersionedValue { block_number: block, value: MaybeDeleted(val) };
        c.append_dup(key, vv).expect("append dup");
    }

    fn append_hashed_storage(
        wtx: &<DatabaseEnv as Database>::TXMut,
        addr: B256,
        slot: B256,
        block: u64,
        val: Option<U256>,
    ) {
        let mut c = wtx.cursor_dup_write::<HashedStorageHistory>().expect("dup write");
        let key = HashedStorageKey::new(addr, slot);
        let vv = VersionedValue { block_number: block, value: MaybeDeleted(val.map(StorageValue)) };
        c.append_dup(key, vv).expect("append dup");
    }

    fn append_hashed_account(
        wtx: &<DatabaseEnv as Database>::TXMut,
        key: B256,
        block: u64,
        val: Option<Account>,
    ) {
        let mut c = wtx.cursor_dup_write::<HashedAccountHistory>().expect("dup write");
        let vv = VersionedValue { block_number: block, value: MaybeDeleted(val) };
        c.append_dup(key, vv).expect("append dup");
    }

    fn upsert_v2_account_current(wtx: &<DatabaseEnv as Database>::TXMut, key: B256, account: Account) {
        wtx.cursor_write::<V2HashedAccounts>()
            .expect("write cursor")
            .upsert(key, &account)
            .expect("upsert account");
    }

    fn upsert_v2_account_history(
        wtx: &<DatabaseEnv as Database>::TXMut,
        key: B256,
        blocks: impl IntoIterator<Item = u64>,
    ) {
        wtx.cursor_write::<V2HashedAccountsHistory>()
            .expect("write cursor")
            .upsert(
                HashedAccountShardedKey::new(key, 0),
                &reth_db::BlockNumberList::new_pre_sorted(blocks),
            )
            .expect("upsert account history");
    }

    fn upsert_v2_account_changeset(
        wtx: &<DatabaseEnv as Database>::TXMut,
        block: u64,
        key: B256,
        account: Option<Account>,
    ) {
        wtx.cursor_dup_write::<V2HashedAccountChangeSets>()
            .expect("dup write cursor")
            .upsert(block, &HashedAccountBeforeTx::new(key, account))
            .expect("upsert account changeset");
    }

    fn upsert_v2_account_trie_current(
        wtx: &<DatabaseEnv as Database>::TXMut,
        path: Nibbles,
        node: BranchNodeCompact,
    ) {
        wtx.cursor_write::<V2AccountsTrie>()
            .expect("write cursor")
            .upsert(StoredNibbles(path), &node)
            .expect("upsert account trie node");
    }

    fn upsert_v2_account_trie_history(
        wtx: &<DatabaseEnv as Database>::TXMut,
        path: Nibbles,
        blocks: impl IntoIterator<Item = u64>,
    ) {
        wtx.cursor_write::<V2AccountsTrieHistory>()
            .expect("write cursor")
            .upsert(
                AccountTrieShardedKey::new(StoredNibbles(path), 0),
                &reth_db::BlockNumberList::new_pre_sorted(blocks),
            )
            .expect("upsert account trie history");
    }

    fn upsert_v2_account_trie_changeset(
        wtx: &<DatabaseEnv as Database>::TXMut,
        block: u64,
        path: Nibbles,
        node: Option<BranchNodeCompact>,
    ) {
        wtx.cursor_dup_write::<V2AccountTrieChangeSets>()
            .expect("dup write cursor")
            .upsert(
                block,
                &TrieChangeSetsEntry {
                    nibbles: StoredNibblesSubKey::from(path),
                    node,
                },
            )
            .expect("upsert account trie changeset");
    }

    fn upsert_v2_storage_current(
        wtx: &<DatabaseEnv as Database>::TXMut,
        address: B256,
        slot: B256,
        value: U256,
    ) {
        wtx.cursor_dup_write::<V2HashedStorages>()
            .expect("dup write cursor")
            .upsert(address, &StorageEntry { key: slot, value })
            .expect("upsert storage current");
    }

    fn upsert_v2_storage_history(
        wtx: &<DatabaseEnv as Database>::TXMut,
        address: B256,
        slot: B256,
        blocks: impl IntoIterator<Item = u64>,
    ) {
        wtx.cursor_write::<V2HashedStoragesHistory>()
            .expect("write cursor")
            .upsert(
                HashedStorageShardedKey::new(address, slot, 0),
                &reth_db::BlockNumberList::new_pre_sorted(blocks),
            )
            .expect("upsert storage history");
    }

    fn upsert_v2_storage_changeset(
        wtx: &<DatabaseEnv as Database>::TXMut,
        block: u64,
        address: B256,
        slot: B256,
        value: U256,
    ) {
        wtx.cursor_dup_write::<V2HashedStorageChangeSets>()
            .expect("dup write cursor")
            .upsert(BlockNumberHashedAddress((block, address)), &StorageEntry { key: slot, value })
            .expect("upsert storage changeset");
    }

    fn upsert_v2_storage_trie_current(
        wtx: &<DatabaseEnv as Database>::TXMut,
        address: B256,
        path: Nibbles,
        node: BranchNodeCompact,
    ) {
        wtx.cursor_dup_write::<V2StoragesTrie>()
            .expect("dup write cursor")
            .upsert(
                address,
                &StorageTrieEntry {
                    nibbles: StoredNibblesSubKey::from(path),
                    node,
                },
            )
            .expect("upsert storage trie current");
    }

    fn upsert_v2_storage_trie_history(
        wtx: &<DatabaseEnv as Database>::TXMut,
        address: B256,
        path: Nibbles,
        blocks: impl IntoIterator<Item = u64>,
    ) {
        wtx.cursor_write::<V2StoragesTrieHistory>()
            .expect("write cursor")
            .upsert(
                StorageTrieShardedKey::new(address, StoredNibbles(path), 0),
                &reth_db::BlockNumberList::new_pre_sorted(blocks),
            )
            .expect("upsert storage trie history");
    }

    fn upsert_v2_storage_trie_changeset(
        wtx: &<DatabaseEnv as Database>::TXMut,
        block: u64,
        address: B256,
        path: Nibbles,
        node: Option<BranchNodeCompact>,
    ) {
        wtx.cursor_dup_write::<V2StorageTrieChangeSets>()
            .expect("dup write cursor")
            .upsert(
                BlockNumberHashedAddress((block, address)),
                &TrieChangeSetsEntry {
                    nibbles: StoredNibblesSubKey::from(path),
                    node,
                },
            )
            .expect("upsert storage trie changeset");
    }

    // Open a dup-RO cursor and wrap it in a BlockNumberVersionedCursor with a given bound.
    fn version_cursor(
        tx: &<DatabaseEnv as Database>::TX,
        max_block: u64,
    ) -> BlockNumberVersionedCursor<AccountTrieHistory, Dup<'_, AccountTrieHistory>> {
        let cur = tx.cursor_dup_read::<AccountTrieHistory>().expect("dup ro cursor");
        BlockNumberVersionedCursor::new(cur, max_block)
    }

    fn account_trie_cursor(
        tx: &'_ <DatabaseEnv as Database>::TX,
        max_block: u64,
    ) -> MdbxTrieCursor<AccountTrieHistory, Dup<'_, AccountTrieHistory>> {
        let c = tx.cursor_dup_read::<AccountTrieHistory>().expect("dup ro cursor");
        // For account trie the address is not used; pass None.
        MdbxTrieCursor::new(c, max_block, None)
    }

    // Helper: build a Storage trie cursor bound to an address
    fn storage_trie_cursor(
        tx: &'_ <DatabaseEnv as Database>::TX,
        max_block: u64,
        address: B256,
    ) -> MdbxTrieCursor<StorageTrieHistory, Dup<'_, StorageTrieHistory>> {
        let c = tx.cursor_dup_read::<StorageTrieHistory>().expect("dup ro cursor");
        MdbxTrieCursor::new(c, max_block, Some(address))
    }

    fn storage_cursor(
        tx: &'_ <DatabaseEnv as Database>::TX,
        max_block: u64,
        address: B256,
    ) -> MdbxStorageCursor<Dup<'_, HashedStorageHistory>> {
        let c = tx.cursor_dup_read::<HashedStorageHistory>().expect("dup ro cursor");
        MdbxStorageCursor::new(c, max_block, address)
    }

    fn account_cursor(
        tx: &'_ <DatabaseEnv as Database>::TX,
        max_block: u64,
    ) -> MdbxAccountCursor<Dup<'_, HashedAccountHistory>> {
        let c = tx.cursor_dup_read::<HashedAccountHistory>().expect("dup ro cursor");
        MdbxAccountCursor::new(c, max_block)
    }

    // Assert helper: ensure the chosen VersionedValue has the expected block and deletion flag.
    fn assert_block(
        got: Option<(StoredNibbles, VersionedValue<BranchNodeCompact>)>,
        expected_block: u64,
        expect_deleted: bool,
    ) {
        let (_, vv) = got.expect("expected Some(..)");
        assert_eq!(vv.block_number, expected_block, "wrong block chosen");
        let is_deleted = matches!(vv.value, MaybeDeleted(None));
        assert_eq!(is_deleted, expect_deleted, "tombstone mismatch");
    }

    /// No entry for key → None.
    #[test]
    fn latest_version_for_key_none_when_key_absent() {
        let db = setup_db();
        let tx = db.tx().expect("ro tx");
        let mut cursor = version_cursor(&tx, 100);

        let out = cursor
            .latest_version_for_key(stored(Nibbles::default()))
            .expect("should not return error");
        assert!(out.is_none(), "absent key must return None");
    }

    /// Exact match at max (live) → pick it.
    #[test]
    fn latest_version_for_key_picks_value_at_max_if_present() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 50, Some(node())); // == max
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 50);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 50, false);
    }

    /// When `seek_by_key_subkey` points to the subkey > max - fallback to the prev.
    #[test]
    fn latest_version_for_key_picks_latest_below_max_when_next_is_above() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 30, Some(node())); // expected
            append_account_trie(&wtx, k.clone(), 70, Some(node())); // > max
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 50);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 30, false);
    }

    /// No ≥ max but key exists → use last < max.
    #[test]
    fn latest_version_for_key_picks_last_below_max_when_none_at_or_above() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 40, Some(node())); // expected (max=100)
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 100);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 40, false);
    }

    /// All entries are > max → None.
    #[test]
    fn latest_version_for_key_none_when_everything_is_above_max() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1.clone(), 60, Some(node()));
            append_account_trie(&wtx, k1.clone(), 70, Some(node()));
            append_account_trie(&wtx, k2, 40, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 50);

        let out = core.latest_version_for_key(k1).expect("ok");
        assert!(out.is_none(), "no dup ≤ max ⇒ None");
    }

    /// Single dup < max → pick it.
    #[test]
    fn latest_version_for_key_picks_single_below_max() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 25, Some(node())); // < max
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 50);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 25, false);
    }

    /// Single dup == max → pick it.
    #[test]
    fn latest_version_for_key_picks_single_at_max() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 50, Some(node())); // == max
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 50);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 50, false);
    }

    /// Latest ≤ max is a tombstone → return it (this API doesn't filter).
    #[test]
    fn latest_version_for_key_returns_tombstone_if_latest_is_deleted() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 90, None); // latest ≤ max, but deleted
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 100);

        let out = core.latest_version_for_key(k).expect("ok");
        assert_block(out, 90, true);
    }

    /// Should skip tombstones and return None when the latest ≤ max is deleted.
    #[test]
    fn seek_exact_skips_tombstone_returns_none() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 90, None); // latest ≤ max is tombstoned
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut core = version_cursor(&tx, 100);

        let out = core.seek_exact(k).expect("ok");
        assert!(out.is_none(), "seek_exact must filter out deleted latest value");
    }

    /// Empty table → None.
    #[test]
    fn seek_empty_returns_none() {
        let db = setup_db();
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        let out = cur.seek(stored(Nibbles::from_nibbles([0x0A]))).expect("ok");
        assert!(out.is_none());
    }

    /// Start at an existing key whose latest ≤ max is live → returns that key.
    #[test]
    fn seek_at_live_key_returns_it() {
        let db = setup_db();
        let k = stored(Nibbles::from_nibbles([0x0A]));
        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k.clone(), 10, Some(node()));
            append_account_trie(&wtx, k.clone(), 20, Some(node())); // latest ≤ max
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 50);

        let out = cur.seek(k.clone()).expect("ok").expect("some");
        assert_eq!(out.0, k);
    }

    /// Start at an existing key whose latest ≤ max is tombstoned → skip to next key with live
    /// value.
    #[test]
    fn seek_skips_tombstoned_key_to_next_live_key() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            // Key 0x10 latest ≤ max is deleted
            append_account_trie(&wtx, k1.clone(), 10, Some(node()));
            append_account_trie(&wtx, k1.clone(), 20, None); // tombstone at latest ≤ max
            // Next key has live
            append_account_trie(&wtx, k2.clone(), 5, Some(node()));
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 50);

        let out = cur.seek(k1).expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// Start between keys → returns the next key’s live latest ≤ max.
    #[test]
    fn seek_between_keys_returns_next_key() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0C]));
        let k3 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1, 10, Some(node()));
            append_account_trie(&wtx, k2.clone(), 10, Some(node()));
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        // Start at 0x15 (between 0x10 and 0x20)

        let out = cur.seek(k3).expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// Start after the last key → None.
    #[test]
    fn seek_after_last_returns_none() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));
        let k3 = stored(Nibbles::from_nibbles([0x0C]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1, 10, Some(node()));
            append_account_trie(&wtx, k2, 10, Some(node()));
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        let out = cur.seek(k3).expect("ok");
        assert!(out.is_none());
    }

    /// If the first key at-or-after has only versions > max, it is effectively not visible → skip
    /// to next.
    #[test]
    fn seek_skips_keys_with_only_versions_above_max() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1.clone(), 60, Some(node()));
            append_account_trie(&wtx, k2.clone(), 40, Some(node()));
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 50);

        let out = cur.seek(k1).expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// Start at a key with mixed versions; latest ≤ max is tombstone → skip to next key with live.
    #[test]
    fn seek_mixed_versions_tombstone_latest_skips_to_next_key() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1.clone(), 10, Some(node()));
            append_account_trie(&wtx, k1.clone(), 30, None);
            append_account_trie(&wtx, k2.clone(), 5, Some(node()));
            wtx.commit().expect("commit");
        }
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 30);

        let out = cur.seek(k1).expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// When not positioned should start from default key and return the first live key.
    #[test]
    fn next_unpositioned_starts_from_default_returns_first_live() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1.clone(), 10, Some(node())); // first live
            append_account_trie(&wtx, k2, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        // Unpositioned cursor
        let mut cur = version_cursor(&tx, 100);

        let out = cur.next().expect("ok").expect("some");
        assert_eq!(out.0, k1);
    }

    /// After positioning on a live key via `seek()`, `next()` should advance to the next live key.
    #[test]
    fn next_advances_from_current_live_to_next_live() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B]));

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1.clone(), 10, Some(node())); // live
            append_account_trie(&wtx, k2.clone(), 10, Some(node())); // next live
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        // Position at k1
        let _ = cur.seek(k1).expect("ok").expect("some");
        // Next should yield k2
        let out = cur.next().expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// If the next key's latest ≤ max is tombstone, `next()` should skip to the next live key.
    #[test]
    fn next_skips_tombstoned_key_to_next_live() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B])); // will be tombstoned at latest ≤ max
        let k3 = stored(Nibbles::from_nibbles([0x0C])); // next live

        {
            let wtx = db.tx_mut().expect("rw tx");
            // k1 live
            append_account_trie(&wtx, k1.clone(), 10, Some(node()));
            // k2: latest ≤ max is tombstone
            append_account_trie(&wtx, k2.clone(), 10, Some(node()));
            append_account_trie(&wtx, k2, 20, None);
            // k3 live
            append_account_trie(&wtx, k3.clone(), 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 50);

        // Position at k1
        let _ = cur.seek(k1).expect("ok").expect("some");
        // next should skip k2 (tombstoned latest) and return k3
        let out = cur.next().expect("ok").expect("some");
        assert_eq!(out.0, k3);
    }

    /// If positioned on the last live key, `next()` should return None (EOF).
    #[test]
    fn next_returns_none_at_eof() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A]));
        let k2 = stored(Nibbles::from_nibbles([0x0B])); // last key

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, k1, 10, Some(node()));
            append_account_trie(&wtx, k2.clone(), 10, Some(node())); // last live
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        // Position at the last key k2
        let _ = cur.seek(k2).expect("ok").expect("some");
        // `next()` should hit EOF
        let out = cur.next().expect("ok");
        assert!(out.is_none());
    }

    /// If the first key has only versions > max, `next()` should skip it and return the next live
    /// key.
    #[test]
    fn next_skips_keys_with_only_versions_above_max() {
        let db = setup_db();
        let k1 = stored(Nibbles::from_nibbles([0x0A])); // only > max
        let k2 = stored(Nibbles::from_nibbles([0x0B])); // ≤ max live

        {
            let wtx = db.tx_mut().expect("rw tx");
            // k1 only above max (max=50)
            append_account_trie(&wtx, k1, 60, Some(node()));
            // k2 within max
            append_account_trie(&wtx, k2.clone(), 40, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        // Unpositioned; `next()` will start from default and walk
        let mut cur = version_cursor(&tx, 50);

        let out = cur.next().expect("ok").expect("some");
        assert_eq!(out.0, k2);
    }

    /// Empty table: `next()` should return None.
    #[test]
    fn next_on_empty_returns_none() {
        let db = setup_db();
        let tx = db.tx().expect("ro tx");
        let mut cur = version_cursor(&tx, 100);

        let out = cur.next().expect("ok");
        assert!(out.is_none());
    }

    // ----------------- Account trie cursor thin-wrapper checks -----------------

    #[test]
    fn account_seek_exact_live_maps_key_and_value() {
        let db = setup_db();
        let k = Nibbles::from_nibbles([0x0A]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, StoredNibbles(k), 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");

        // Build wrapper
        let mut cur = account_trie_cursor(&tx, 100);

        // Wrapper should return (Nibbles, BranchNodeCompact)
        let out = TrieCursor::seek_exact(&mut cur, k).expect("ok").expect("some");
        assert_eq!(out.0, k);
    }

    #[test]
    fn account_seek_exact_filters_tombstone() {
        let db = setup_db();
        let k = Nibbles::from_nibbles([0x0B]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, StoredNibbles(k), 5, Some(node()));
            append_account_trie(&wtx, StoredNibbles(k), 9, None); // latest ≤ max tombstone
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = account_trie_cursor(&tx, 10);

        let out = TrieCursor::seek_exact(&mut cur, k).expect("ok");
        assert!(out.is_none(), "account seek_exact must filter tombstone");
    }

    #[test]
    fn account_seek_exact_miss_does_not_skip_next_row() {
        let db = setup_db();
        let missing = Nibbles::from_nibbles([0x01]);
        let present = Nibbles::from_nibbles([0x02]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, StoredNibbles(present), 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = account_trie_cursor(&tx, 100);

        assert!(TrieCursor::seek_exact(&mut cur, missing).expect("ok").is_none());
        let out = TrieCursor::next(&mut cur).expect("ok").expect("next row");
        assert_eq!(out.0, present);
    }

    #[test]
    fn account_seek_and_next_and_current_roundtrip() {
        let db = setup_db();
        let k1 = Nibbles::from_nibbles([0x01]);
        let k2 = Nibbles::from_nibbles([0x02]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_account_trie(&wtx, StoredNibbles(k1), 10, Some(node()));
            append_account_trie(&wtx, StoredNibbles(k2), 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = account_trie_cursor(&tx, 100);

        // seek at k1
        let out1 = TrieCursor::seek(&mut cur, k1).expect("ok").expect("some");
        assert_eq!(out1.0, k1);

        // current should be k1
        let cur_k = TrieCursor::current(&mut cur).expect("ok").expect("some");
        assert_eq!(cur_k, k1);

        // next should move to k2
        let out2 = TrieCursor::next(&mut cur).expect("ok").expect("some");
        assert_eq!(out2.0, k2);
    }

    // ----------------- Storage trie cursor thin-wrapper checks -----------------

    #[test]
    fn storage_seek_exact_respects_address_filter() {
        let db = setup_db();

        let addr_a = B256::from([0xAA; 32]);
        let addr_b = B256::from([0xBB; 32]);

        let path = Nibbles::from_nibbles([0x0D]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            // insert only under B
            append_storage_trie(&wtx, addr_b, path, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");

        // Cursor bound to A must not see B’s data
        let mut cur_a = storage_trie_cursor(&tx, 100, addr_a);
        let out_a = TrieCursor::seek_exact(&mut cur_a, path).expect("ok");
        assert!(out_a.is_none(), "no data for addr A");

        // Cursor bound to B should see it
        let mut cur_b = storage_trie_cursor(&tx, 100, addr_b);
        let out_b = TrieCursor::seek_exact(&mut cur_b, path).expect("ok").expect("some");
        assert_eq!(out_b.0, path);
    }

    #[test]
    fn storage_seek_returns_first_key_for_bound_address() {
        let db = setup_db();

        let addr_a = B256::from([0x11; 32]);
        let addr_b = B256::from([0x22; 32]);

        let p1 = Nibbles::from_nibbles([0x01]);
        let p2 = Nibbles::from_nibbles([0x02]);
        let p3 = Nibbles::from_nibbles([0x03]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            // For A: only p2
            append_storage_trie(&wtx, addr_a, p2, 10, Some(node()));
            // For B: p1
            append_storage_trie(&wtx, addr_b, p1, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        // test seek behaviour
        {
            let tx = db.tx().expect("ro tx");
            let mut cur_a = storage_trie_cursor(&tx, 100, addr_a);

            // seek at p1: for A there is no p1; the next key >= p1 under A is p2
            let out = TrieCursor::seek(&mut cur_a, p1).expect("ok").expect("some");
            assert_eq!(out.0, p2);

            // seek at p2: exact match
            let out = TrieCursor::seek(&mut cur_a, p2).expect("ok").expect("some");
            assert_eq!(out.0, p2);

            // seek at p3: no p3 under A; no next key ≥ p3 under A → None
            let out = TrieCursor::seek(&mut cur_a, p3).expect("ok");
            assert!(out.is_none(), "no key ≥ p3 under A");
        }

        // test next behaviour
        {
            let tx = db.tx().expect("ro tx");
            let mut cur_a = storage_trie_cursor(&tx, 100, addr_a);

            let out = TrieCursor::next(&mut cur_a).expect("ok").expect("some");
            assert_eq!(out.0, p2);

            // next should yield None as there is no further key under A
            let out = TrieCursor::next(&mut cur_a).expect("ok");
            assert!(out.is_none(), "no more keys under A");

            // current should return None
            let out = TrieCursor::current(&mut cur_a).expect("ok");
            assert!(out.is_none(), "no current key after EOF");
        }

        // test seek_exact behaviour
        {
            let tx = db.tx().expect("ro tx");
            let mut cur_a = storage_trie_cursor(&tx, 100, addr_a);

            // seek_exact at p1: no exact match
            let out = TrieCursor::seek_exact(&mut cur_a, p1).expect("ok");
            assert!(out.is_none(), "no exact p1 under A");

            // seek_exact at p2: exact match
            let out = TrieCursor::seek_exact(&mut cur_a, p2).expect("ok").expect("some");
            assert_eq!(out.0, p2);

            // seek_exact at p3: no exact match
            let out = TrieCursor::seek_exact(&mut cur_a, p3).expect("ok");
            assert!(out.is_none(), "no exact p3 under A");
        }
    }

    #[test]
    fn storage_seek_exact_miss_does_not_skip_next_row() {
        let db = setup_db();
        let addr = B256::from([0x23; 32]);
        let missing = Nibbles::from_nibbles([0x01]);
        let present = Nibbles::from_nibbles([0x02]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_storage_trie(&wtx, addr, present, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = storage_trie_cursor(&tx, 100, addr);

        assert!(TrieCursor::seek_exact(&mut cur, missing).expect("ok").is_none());
        let out = TrieCursor::next(&mut cur).expect("ok").expect("next row");
        assert_eq!(out.0, present);
    }

    #[test]
    fn storage_next_stops_at_address_boundary() {
        let db = setup_db();

        let addr_a = B256::from([0x33; 32]);
        let addr_b = B256::from([0x44; 32]);

        let p1 = Nibbles::from_nibbles([0x05]); // under A
        let p2 = Nibbles::from_nibbles([0x06]); // under B (next key overall)

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_storage_trie(&wtx, addr_a, p1, 10, Some(node()));
            append_storage_trie(&wtx, addr_b, p2, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur_a = storage_trie_cursor(&tx, 100, addr_a);

        // position at p1 (A)
        let _ = TrieCursor::seek_exact(&mut cur_a, p1).expect("ok").expect("some");

        // next should reach boundary; impl filters different address and returns None
        let out = TrieCursor::next(&mut cur_a).expect("ok");
        assert!(out.is_none(), "next() should stop when next key is a different address");
    }

    #[test]
    fn storage_current_maps_key() {
        let db = setup_db();

        let addr = B256::from([0x55; 32]);
        let p = Nibbles::from_nibbles([0x09]);

        {
            let wtx = db.tx_mut().expect("rw tx");
            append_storage_trie(&wtx, addr, p, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro tx");
        let mut cur = storage_trie_cursor(&tx, 100, addr);

        let _ = TrieCursor::seek_exact(&mut cur, p).expect("ok").expect("some");

        let now = TrieCursor::current(&mut cur).expect("ok").expect("some");
        assert_eq!(now, p);
    }

    #[test]
    fn hashed_storage_seek_maps_slot_and_value() {
        let db = setup_db();
        let addr = B256::from([0xAA; 32]);
        let slot = B256::from([0x10; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_storage(&wtx, addr, slot, 10, Some(U256::from(7)));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = storage_cursor(&tx, 100, addr);

        let (got_slot, got_val) = cur.seek(slot).expect("ok").expect("some");
        assert_eq!(got_slot, slot);
        assert_eq!(got_val, U256::from(7));
    }

    #[test]
    fn hashed_storage_seek_filters_tombstone() {
        let db = setup_db();
        let addr = B256::from([0xAB; 32]);
        let slot = B256::from([0x11; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_storage(&wtx, addr, slot, 5, Some(U256::from(1)));
            append_hashed_storage(&wtx, addr, slot, 9, None); // latest ≤ max is tombstone
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = storage_cursor(&tx, 10, addr);

        let out = cur.seek(slot).expect("ok");
        assert!(out.is_none(), "wrapper must filter tombstoned latest");
    }

    #[test]
    fn hashed_storage_seek_and_next_roundtrip() {
        let db = setup_db();
        let addr = B256::from([0xAC; 32]);
        let s1 = B256::from([0x01; 32]);
        let s2 = B256::from([0x02; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_storage(&wtx, addr, s1, 10, Some(U256::from(11)));
            append_hashed_storage(&wtx, addr, s2, 10, Some(U256::from(22)));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = storage_cursor(&tx, 100, addr);

        let (k1, v1) = cur.seek(s1).expect("ok").expect("some");
        assert_eq!((k1, v1), (s1, U256::from(11)));

        let (k2, v2) = cur.next().expect("ok").expect("some");
        assert_eq!((k2, v2), (s2, U256::from(22)));
    }

    #[test]
    fn hashed_storage_address_boundary() {
        let db = setup_db();
        let addr1 = B256::from([0xAC; 32]);
        let addr2 = B256::from([0xAD; 32]);
        let s1 = B256::from([0x01; 32]);
        let s2 = B256::from([0x02; 32]);
        let s3 = B256::from([0x03; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_storage(&wtx, addr1, s1, 10, Some(U256::from(11)));
            append_hashed_storage(&wtx, addr1, s2, 10, Some(U256::from(22)));
            wtx.commit().expect("commit");
        }

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_storage(&wtx, addr2, s1, 10, Some(U256::from(33)));
            append_hashed_storage(&wtx, addr2, s2, 10, Some(U256::from(44)));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = storage_cursor(&tx, 100, addr1);

        let (k1, v1) = cur.next().expect("ok").expect("some");
        assert_eq!((k1, v1), (s1, U256::from(11)));

        let (k2, v2) = cur.next().expect("ok").expect("some");
        assert_eq!((k2, v2), (s2, U256::from(22)));

        let out = cur.next().expect("ok");
        assert!(out.is_none(), "should stop at address boundary");

        let (k1, v1) = cur.seek(s1).expect("ok").expect("some");
        assert_eq!((k1, v1), (s1, U256::from(11)));

        let (k2, v2) = cur.seek(s2).expect("ok").expect("some");
        assert_eq!((k2, v2), (s2, U256::from(22)));

        let out = cur.seek(s3).expect("ok");
        assert!(out.is_none(), "should not see keys from other address");
    }

    #[test]
    fn hashed_account_seek_maps_key_and_value() {
        let db = setup_db();
        let key = B256::from([0x20; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_account(&wtx, key, 10, Some(Account::default()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = account_cursor(&tx, 100);

        let (got_key, _acc) = cur.seek(key).expect("ok").expect("some");
        assert_eq!(got_key, key);
    }

    #[test]
    fn hashed_account_seek_filters_tombstone() {
        let db = setup_db();
        let key = B256::from([0x21; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_account(&wtx, key, 5, Some(Account::default()));
            append_hashed_account(&wtx, key, 9, None); // latest ≤ max is tombstone
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = account_cursor(&tx, 10);

        let out = cur.seek(key).expect("ok");
        assert!(out.is_none(), "wrapper must filter tombstoned latest");
    }

    #[test]
    fn hashed_account_seek_and_next_roundtrip() {
        let db = setup_db();
        let k1 = B256::from([0x01; 32]);
        let k2 = B256::from([0x02; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_hashed_account(&wtx, k1, 10, Some(Account::default()));
            append_hashed_account(&wtx, k2, 10, Some(Account::default()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cur = account_cursor(&tx, 100);

        let (got1, _) = cur.seek(k1).expect("ok").expect("some");
        assert_eq!(got1, k1);

        let (got2, _) = cur.next().expect("ok").expect("some");
        assert_eq!(got2, k2);
    }

    /// Regression test: `MdbxStorageCursor` `next()` should work without explicit `seek()`
    /// when cursor is constructed for a non-first key.
    ///
    /// Bug: When a storage cursor is created for a specific address (e.g., 0x02),
    /// calling `next()` without first calling `seek()` returns None instead of the first
    /// slot for that address. This only manifests when the address is not the first
    /// in the table.
    #[test]
    fn storage_cursor_next_without_seek_for_non_first_address() {
        let db = setup_db();
        let addr1 = B256::from([0x01; 32]); // First address
        let addr2 = B256::from([0x02; 32]); // Second address (non-first)
        let slot1 = B256::from([0x11; 32]);
        let slot2 = B256::from([0x12; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            // Add storage for first address
            append_hashed_storage(&wtx, addr1, slot1, 10, Some(U256::from(100)));

            // Add storage for second address
            append_hashed_storage(&wtx, addr2, slot2, 10, Some(U256::from(200)));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");

        // Test with addr1 (first address) - this typically works
        let mut cur1 = storage_cursor(&tx, 100, addr1);
        let result1 = cur1.next().expect("ok");
        assert!(result1.is_some(), "next() should return data for first address without seek()");
        if let Some((key, val)) = result1 {
            assert_eq!(key, slot1);
            assert_eq!(val, U256::from(100));
        }

        // Test with addr2 (non-first address) - this demonstrates the bug fix
        let mut cur2 = storage_cursor(&tx, 100, addr2);
        let result2_without_seek = cur2.next().expect("ok");

        assert!(
            result2_without_seek.is_some(),
            "next() should return data for non-first address without seek()"
        );
        if let Some((key, val)) = result2_without_seek {
            assert_eq!(key, slot2);
            assert_eq!(val, U256::from(200));
        }

        // Verify that seek() works correctly
        let mut cur3 = storage_cursor(&tx, 100, addr2);
        let result3_with_seek = cur3.seek(slot2).expect("ok");
        assert!(result3_with_seek.is_some(), "seek() should find the slot for addr2");
        if let Some((key, val)) = result3_with_seek {
            assert_eq!(key, slot2);
            assert_eq!(val, U256::from(200));
        }
    }

    /// Regression test: `MdbxTrieCursor`<StorageTrieHistory> `next()` should work without `seek()`
    /// for non-first addresses.
    #[test]
    fn storage_trie_cursor_next_without_seek_for_non_first_address() {
        let db = setup_db();
        let addr1 = B256::from([0x01; 32]);
        let addr2 = B256::from([0x02; 32]);
        let path1 = Nibbles::from_nibbles([0x0A]);
        let path2 = Nibbles::from_nibbles([0x0B]);

        {
            let wtx = db.tx_mut().expect("rw");
            append_storage_trie(&wtx, addr1, path1, 10, Some(node()));
            append_storage_trie(&wtx, addr2, path2, 10, Some(node()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");

        // Test addr1 (first) - works
        let mut cur1 = storage_trie_cursor(&tx, 100, addr1);
        let result1 = TrieCursor::next(&mut cur1).expect("ok");
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().0, path1);

        // Test addr2 (non-first) - should also work now
        let mut cur2 = storage_trie_cursor(&tx, 100, addr2);
        let result2 = TrieCursor::next(&mut cur2).expect("ok");
        assert!(result2.is_some(), "next() should work for non-first address without seek()");
        assert_eq!(result2.unwrap().0, path2);
    }

    #[test]
    fn latest_account_cursor_iterates_current_table() {
        let db = setup_db();
        let k1 = B256::from([0x01; 32]);
        let k2 = B256::from([0x02; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_write::<V2HashedAccounts>().expect("write cursor");
            cursor.upsert(k1, &Account::default()).expect("insert account");
            cursor.upsert(k2, &Account::default()).expect("insert account");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor =
            MdbxV2LatestAccountCursor::new(tx.cursor_read::<V2HashedAccounts>().expect("read"));

        let (got1, _) = cursor.seek(k1).expect("ok").expect("first account");
        assert_eq!(got1, k1);

        let (got2, _) = cursor.next().expect("ok").expect("second account");
        assert_eq!(got2, k2);
    }

    #[test]
    fn latest_account_trie_cursor_iterates_current_table() {
        let db = setup_db();
        let p1 = Nibbles::from_nibbles([0x01]);
        let p2 = Nibbles::from_nibbles([0x02]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_write::<V2AccountsTrie>().expect("write cursor");
            cursor.upsert(StoredNibbles(p1), &node()).expect("insert trie node");
            cursor.upsert(StoredNibbles(p2), &node()).expect("insert trie node");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor =
            MdbxV2LatestAccountTrieCursor::new(tx.cursor_read::<V2AccountsTrie>().expect("read"));

        let (got1, _) = cursor.seek(p1).expect("ok").expect("first trie node");
        assert_eq!(got1, p1);

        let (got2, _) = cursor.next().expect("ok").expect("second trie node");
        assert_eq!(got2, p2);
    }

    #[test]
    fn latest_storage_cursor_uses_dupsort_and_stops_at_address_boundary() {
        let db = setup_db();
        let addr1 = B256::from([0x01; 32]);
        let addr2 = B256::from([0x02; 32]);
        let s1 = B256::from([0x11; 32]);
        let s2 = B256::from([0x12; 32]);
        let s3 = B256::from([0x13; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_dup_write::<V2HashedStorages>().expect("dup write");
            cursor
                .upsert(addr1, &StorageEntry { key: s1, value: U256::from(11) })
                .expect("insert storage");
            cursor
                .upsert(addr1, &StorageEntry { key: s1, value: U256::from(111) })
                .expect("insert duplicate storage");
            cursor
                .upsert(addr1, &StorageEntry { key: s2, value: U256::from(22) })
                .expect("insert storage");
            cursor
                .upsert(addr2, &StorageEntry { key: s3, value: U256::from(33) })
                .expect("insert storage");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2LatestStorageCursor::new(
            tx.cursor_dup_read::<V2HashedStorages>().expect("read"),
            addr1,
        );

        let (got1, _) = cursor.seek(s1).expect("ok").expect("first slot");
        assert_eq!(got1, s1);

        let (got2, value2) = cursor.next().expect("ok").expect("second slot");
        assert_eq!((got2, value2), (s2, U256::from(22)));

        let out = cursor.next().expect("ok");
        assert!(out.is_none(), "should stop at address boundary");

        let out = cursor.seek(s3).expect("ok");
        assert!(out.is_none(), "should not expose another address slot");
    }

    #[test]
    fn historical_v2_storage_cursor_can_switch_addresses() {
        let db = setup_db();
        let addr1 = B256::from([0x31; 32]);
        let addr2 = B256::from([0x32; 32]);
        let s1 = B256::from([0x41; 32]);
        let s2 = B256::from([0x42; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_dup_write::<V2HashedStorages>().expect("dup write");
            cursor
                .upsert(addr1, &StorageEntry { key: s1, value: U256::from(11) })
                .expect("insert addr1 storage");
            cursor
                .upsert(addr2, &StorageEntry { key: s2, value: U256::from(22) })
                .expect("insert addr2 storage");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2StorageCursor::new(&tx, 100, addr1).expect("historical cursor");
        assert_eq!(cursor.seek(s1).expect("addr1 seek").expect("addr1 row"), (s1, U256::from(11)));

        cursor.set_hashed_address(addr2);
        assert_eq!(cursor.seek(s2).expect("addr2 seek").expect("addr2 row"), (s2, U256::from(22)));
    }

    #[test]
    fn historical_v2_account_cursor_merges_current_and_changesets() {
        let db = setup_db();
        let account1 = B256::from([0x41; 32]);
        let account2 = B256::from([0x42; 32]);
        let account3 = B256::from([0x43; 32]);
        let old1 = Account { nonce: 1, ..Default::default() };
        let old2 = Account { nonce: 2, ..Default::default() };
        let current2 = Account { nonce: 20, ..Default::default() };
        let current3 = Account { nonce: 3, ..Default::default() };

        {
            let wtx = db.tx_mut().expect("rw");
            upsert_v2_account_current(&wtx, account2, current2);
            upsert_v2_account_current(&wtx, account3, current3);
            upsert_v2_account_history(&wtx, account1, [20]);
            upsert_v2_account_history(&wtx, account2, [20]);
            upsert_v2_account_changeset(&wtx, 20, account1, Some(old1));
            upsert_v2_account_changeset(&wtx, 20, account2, Some(old2));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2AccountCursor::new(&tx, 10).expect("historical cursor");

        assert_eq!(cursor.seek(account1).expect("seek").expect("row"), (account1, old1));
        assert_eq!(cursor.next().expect("next").expect("row"), (account2, old2));
        assert_eq!(cursor.next().expect("next").expect("row"), (account3, current3));
        assert!(cursor.next().expect("eof").is_none());
    }

    #[test]
    fn historical_v2_account_trie_cursor_merges_current_and_changesets() {
        let db = setup_db();
        let path1 = Nibbles::from_nibbles([0x04]);
        let path2 = Nibbles::from_nibbles([0x05]);
        let path3 = Nibbles::from_nibbles([0x06]);
        let old1 = node();
        let old2 = node();
        let current2 = node();
        let current3 = node();

        {
            let wtx = db.tx_mut().expect("rw");
            upsert_v2_account_trie_current(&wtx, path2, current2.clone());
            upsert_v2_account_trie_current(&wtx, path3, current3.clone());
            upsert_v2_account_trie_history(&wtx, path1, [20]);
            upsert_v2_account_trie_history(&wtx, path2, [20]);
            upsert_v2_account_trie_changeset(&wtx, 20, path1, Some(old1.clone()));
            upsert_v2_account_trie_changeset(&wtx, 20, path2, Some(old2.clone()));
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2AccountTrieCursor::new(&tx, 10).expect("historical cursor");

        assert_eq!(TrieCursor::seek(&mut cursor, path1).expect("seek").expect("row"), (path1, old1));
        assert_eq!(TrieCursor::next(&mut cursor).expect("next").expect("row"), (path2, old2));
        assert_eq!(TrieCursor::next(&mut cursor).expect("next").expect("row"), (path3, current3));
        assert!(TrieCursor::next(&mut cursor).expect("eof").is_none());
    }

    #[test]
    fn historical_v2_storage_cursor_merges_current_and_changesets() {
        let db = setup_db();
        let addr = B256::from([0x51; 32]);
        let slot1 = B256::from([0x61; 32]);
        let slot2 = B256::from([0x62; 32]);
        let slot3 = B256::from([0x63; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            upsert_v2_storage_current(&wtx, addr, slot2, U256::from(20));
            upsert_v2_storage_current(&wtx, addr, slot3, U256::from(3));
            upsert_v2_storage_history(&wtx, addr, slot1, [20]);
            upsert_v2_storage_history(&wtx, addr, slot2, [20]);
            upsert_v2_storage_history(&wtx, addr, B256::from([0x64; 32]), [20]);
            upsert_v2_storage_changeset(&wtx, 20, addr, slot1, U256::from(1));
            upsert_v2_storage_changeset(&wtx, 20, addr, slot2, U256::from(2));
            upsert_v2_storage_changeset(&wtx, 20, addr, B256::from([0x64; 32]), U256::ZERO);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2StorageCursor::new(&tx, 10, addr).expect("historical cursor");

        assert_eq!(cursor.seek(slot1).expect("seek").expect("row"), (slot1, U256::from(1)));
        assert_eq!(cursor.next().expect("next").expect("row"), (slot2, U256::from(2)));
        assert_eq!(cursor.next().expect("next").expect("row"), (slot3, U256::from(3)));
        assert!(cursor.next().expect("eof").is_none());
    }

    #[test]
    fn historical_v2_storage_trie_cursor_merges_current_and_changesets() {
        let db = setup_db();
        let addr = B256::from([0x71; 32]);
        let path1 = Nibbles::from_nibbles([0x07]);
        let path2 = Nibbles::from_nibbles([0x08]);
        let path3 = Nibbles::from_nibbles([0x09]);
        let old1 = node();
        let old2 = node();
        let current3 = node();

        {
            let wtx = db.tx_mut().expect("rw");
            upsert_v2_storage_trie_current(&wtx, addr, path2, node());
            upsert_v2_storage_trie_current(&wtx, addr, path3, current3.clone());
            upsert_v2_storage_trie_history(&wtx, addr, path1, [20]);
            upsert_v2_storage_trie_history(&wtx, addr, path2, [20]);
            upsert_v2_storage_trie_history(&wtx, addr, Nibbles::from_nibbles([0x0A]), [20]);
            upsert_v2_storage_trie_changeset(&wtx, 20, addr, path1, Some(old1.clone()));
            upsert_v2_storage_trie_changeset(&wtx, 20, addr, path2, Some(old2.clone()));
            upsert_v2_storage_trie_changeset(&wtx, 20, addr, Nibbles::from_nibbles([0x0A]), None);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2StorageTrieCursor::new(&tx, 10, addr).expect("historical cursor");

        assert_eq!(TrieCursor::seek(&mut cursor, path1).expect("seek").expect("row"), (path1, old1));
        assert_eq!(TrieCursor::next(&mut cursor).expect("next").expect("row"), (path2, old2));
        assert_eq!(TrieCursor::next(&mut cursor).expect("next").expect("row"), (path3, current3));
        assert!(TrieCursor::next(&mut cursor).expect("eof").is_none());
    }

    #[test]
    fn latest_storage_cursor_skips_zero_rows_iteratively() {
        let db = setup_db();
        let addr = B256::from([0x01; 32]);
        let zero_slot = B256::from([0x11; 32]);
        let live_slot = B256::from([0x12; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_dup_write::<V2HashedStorages>().expect("dup write");
            cursor
                .upsert(addr, &StorageEntry { key: zero_slot, value: U256::ZERO })
                .expect("insert zero storage");
            cursor
                .upsert(addr, &StorageEntry { key: live_slot, value: U256::from(22) })
                .expect("insert live storage");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2LatestStorageCursor::new(
            tx.cursor_dup_read::<V2HashedStorages>().expect("read"),
            addr,
        );

        let (got, value) = cursor.seek(zero_slot).expect("ok").expect("live slot");
        assert_eq!((got, value), (live_slot, U256::from(22)));
        assert!(cursor.next().expect("ok").is_none());
    }

    #[test]
    fn latest_account_cursor_reset_restarts_iteration() {
        let db = setup_db();
        let account1 = B256::from([0x11; 32]);
        let account2 = B256::from([0x12; 32]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_write::<V2HashedAccounts>().expect("write");
            cursor
                .upsert(account1, &Account { nonce: 1, ..Default::default() })
                .expect("insert account1");
            cursor
                .upsert(account2, &Account { nonce: 2, ..Default::default() })
                .expect("insert account2");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor =
            MdbxV2LatestAccountCursor::new(tx.cursor_read::<V2HashedAccounts>().expect("read"));

        assert_eq!(cursor.next().expect("first").expect("row").0, account1);
        assert_eq!(cursor.next().expect("second").expect("row").0, account2);
        assert!(cursor.next().expect("eof").is_none());

        cursor.reset();

        assert_eq!(cursor.next().expect("reset first").expect("row").0, account1);
    }

    #[test]
    fn latest_storage_trie_cursor_uses_dupsort_and_stops_at_address_boundary() {
        let db = setup_db();
        let addr1 = B256::from([0x01; 32]);
        let addr2 = B256::from([0x02; 32]);
        let p1 = Nibbles::from_nibbles([0x01]);
        let p2 = Nibbles::from_nibbles([0x02]);
        let p3 = Nibbles::from_nibbles([0x03]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_dup_write::<V2StoragesTrie>().expect("dup write");
            cursor
                .upsert(
                    addr1,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p1), node: node() },
                )
                .expect("insert storage trie");
            cursor
                .upsert(
                    addr1,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p1), node: node() },
                )
                .expect("insert duplicate storage trie");
            cursor
                .upsert(
                    addr1,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p2), node: node() },
                )
                .expect("insert storage trie");
            cursor
                .upsert(
                    addr2,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p3), node: node() },
                )
                .expect("insert storage trie");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2LatestStorageTrieCursor::new(
            tx.cursor_dup_read::<V2StoragesTrie>().expect("read"),
            addr1,
        );

        let (got1, _) = cursor.seek(p1).expect("ok").expect("first path");
        assert_eq!(got1, p1);

        let (got2, _) = cursor.next().expect("ok").expect("second path");
        assert_eq!(got2, p2);

        let out = cursor.next().expect("ok");
        assert!(out.is_none(), "should stop at address boundary");

        let out = cursor.seek(p3).expect("ok");
        assert!(out.is_none(), "should not expose another address path");
    }

    #[test]
    fn latest_account_trie_cursor_reset_restarts_iteration() {
        let db = setup_db();
        let p1 = StoredNibbles(Nibbles::from_nibbles([0x01]));
        let p2 = StoredNibbles(Nibbles::from_nibbles([0x02]));

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_write::<V2AccountsTrie>().expect("write");
            cursor.upsert(p1.clone(), &node()).expect("insert p1");
            cursor.upsert(p2.clone(), &node()).expect("insert p2");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2LatestAccountTrieCursor::new(
            tx.cursor_read::<V2AccountsTrie>().expect("read"),
        );

        assert_eq!(cursor.next().expect("first").expect("row").0, p1.0);
        assert_eq!(cursor.next().expect("second").expect("row").0, p2.0);
        assert!(cursor.next().expect("eof").is_none());

        cursor.reset();

        assert_eq!(cursor.next().expect("reset first").expect("row").0, p1.0);
        assert_eq!(cursor.current().expect("current after reset"), Some(p1.0));
    }

    #[test]
    fn historical_v2_storage_trie_cursor_can_switch_addresses() {
        let db = setup_db();
        let addr1 = B256::from([0x51; 32]);
        let addr2 = B256::from([0x52; 32]);
        let p1 = Nibbles::from_nibbles([0x06]);
        let p2 = Nibbles::from_nibbles([0x07]);

        {
            let wtx = db.tx_mut().expect("rw");
            let mut cursor = wtx.cursor_dup_write::<V2StoragesTrie>().expect("dup write");
            cursor
                .upsert(
                    addr1,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p1), node: node() },
                )
                .expect("insert addr1 storage trie");
            cursor
                .upsert(
                    addr2,
                    &StorageTrieEntry { nibbles: StoredNibblesSubKey::from(p2), node: node() },
                )
                .expect("insert addr2 storage trie");
            drop(cursor);
            wtx.commit().expect("commit");
        }

        let tx = db.tx().expect("ro");
        let mut cursor = MdbxV2StorageTrieCursor::new(&tx, 100, addr1).expect("historical cursor");
        assert_eq!(cursor.seek(p1).expect("addr1 seek").expect("addr1 row").0, p1);

        cursor.set_hashed_address(addr2);
        assert_eq!(cursor.seek(p2).expect("addr2 seek").expect("addr2 row").0, p2);
    }
}
