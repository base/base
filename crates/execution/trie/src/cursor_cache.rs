//! Request-scoped memoization of trie and hashed cursor results.
//!
//! Every cursor opened against one read snapshot at one max block number returns the same result
//! for the same positioning operation, so cursors opened by concurrent proof jobs can share their
//! results. [`CursorResultCache`] holds those results for a single request, and the
//! [`CachedTrieCursorFactory`] / [`CachedHashedCursorFactory`] wrappers consult it before reading
//! from the wrapped factory's cursors.

use core::{
    fmt::Debug,
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

use alloy_primitives::{B256, U256, map::DefaultHashBuilder};
use dashmap::DashMap;
use reth_db::DatabaseError;
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedCursorFactory, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieCursorFactory, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles};

/// A cursor positioning operation whose result depends only on the snapshot and its input key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CursorOp<K> {
    /// `seek(key)`.
    Seek(K),
    /// `seek_exact(key)`.
    SeekExact(K),
    /// `next()` while positioned on `key`.
    Next(K),
}

/// Memoized results for one cursor kind, keyed by storage-trie address (`None` for the account
/// tries) and operation.
pub type CursorResultMap<K, V> =
    DashMap<(Option<B256>, CursorOp<K>), Option<(K, V)>, DefaultHashBuilder>;

/// Cursor results shared by every cursor opened through the cached factories of one request.
///
/// Entries are never invalidated, so a cache must only span cursors that read the same snapshot
/// at the same max block number.
#[derive(Debug, Default)]
pub struct CursorResultCache {
    trie_nodes: CursorResultMap<Nibbles, BranchNodeCompact>,
    hashed_accounts: CursorResultMap<B256, Account>,
    hashed_storages: CursorResultMap<B256, U256>,
    empty_storages: DashMap<B256, bool, DefaultHashBuilder>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CursorResultCache {
    /// Returns the number of cursor operations served from the cache by cursors dropped so far.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Returns the number of cursor operations that read through to the wrapped cursors, counted
    /// like [`Self::hits`].
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

/// Per-cursor memoization state: the logical position, the cache-served operation, if any, that
/// the wrapped cursor has not applied yet, and hit/miss counts flushed to the cache on drop so
/// concurrent cursors do not contend on the shared counters.
#[derive(Debug)]
pub struct CursorMemo<'c, K: Eq + Hash, V> {
    cache: &'c CursorResultCache,
    results: &'c CursorResultMap<K, V>,
    hashed_address: Option<B256>,
    position: Option<K>,
    pending: Option<CursorOp<K>>,
    hits: u64,
    misses: u64,
}

impl<'c, K, V> CursorMemo<'c, K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    /// Creates the state for a fresh, unpositioned cursor.
    pub const fn new(
        cache: &'c CursorResultCache,
        results: &'c CursorResultMap<K, V>,
        hashed_address: Option<B256>,
    ) -> Self {
        Self { cache, results, hashed_address, position: None, pending: None, hits: 0, misses: 0 }
    }

    /// Returns the cached result of `op` and moves the logical position to it.
    pub fn get(&mut self, op: &CursorOp<K>) -> Option<Option<(K, V)>> {
        let result = self.results.get(&(self.hashed_address, op.clone()))?.value().clone();
        self.hits += 1;
        self.position = result.as_ref().map(|(key, _)| key.clone());
        self.pending = Some(op.clone());
        Some(result)
    }

    /// Records `result`, which the wrapped cursor returned for `op` and is now positioned at.
    pub fn insert(&mut self, op: CursorOp<K>, result: &Option<(K, V)>) {
        self.misses += 1;
        self.position = result.as_ref().map(|(key, _)| key.clone());
        self.pending = None;
        self.results.insert((self.hashed_address, op), result.clone());
    }

    /// Marks the cursor unpositioned, for example after a reset or address change.
    pub fn unposition(&mut self, hashed_address: Option<B256>) {
        self.hashed_address = hashed_address;
        self.position = None;
        self.pending = None;
    }
}

impl<K: Eq + Hash, V> Drop for CursorMemo<'_, K, V> {
    fn drop(&mut self) {
        self.cache.hits.fetch_add(self.hits, Ordering::Relaxed);
        self.cache.misses.fetch_add(self.misses, Ordering::Relaxed);
    }
}

/// [`TrieCursorFactory`] whose cursors memoize results in a shared [`CursorResultCache`].
#[derive(Debug, Clone)]
pub struct CachedTrieCursorFactory<'c, F> {
    inner: F,
    cache: &'c CursorResultCache,
}

impl<'c, F> CachedTrieCursorFactory<'c, F> {
    /// Wraps `inner` so its cursors share `cache`.
    pub const fn new(inner: F, cache: &'c CursorResultCache) -> Self {
        Self { inner, cache }
    }
}

impl<F: TrieCursorFactory> TrieCursorFactory for CachedTrieCursorFactory<'_, F> {
    type AccountTrieCursor<'a>
        = CachedTrieCursor<'a, F::AccountTrieCursor<'a>>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = CachedTrieCursor<'a, F::StorageTrieCursor<'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(CachedTrieCursor {
            inner: self.inner.account_trie_cursor()?,
            memo: CursorMemo::new(self.cache, &self.cache.trie_nodes, None),
        })
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(CachedTrieCursor {
            inner: self.inner.storage_trie_cursor(hashed_address)?,
            memo: CursorMemo::new(self.cache, &self.cache.trie_nodes, Some(hashed_address)),
        })
    }
}

/// [`TrieCursor`] that serves repeated operations from a [`CursorResultCache`].
#[derive(Debug)]
pub struct CachedTrieCursor<'c, C> {
    inner: C,
    memo: CursorMemo<'c, Nibbles, BranchNodeCompact>,
}

impl<C: TrieCursor> CachedTrieCursor<'_, C> {
    /// Applies the last cache-served operation to the wrapped cursor.
    fn sync(&mut self) -> Result<(), DatabaseError> {
        match self.memo.pending.take() {
            Some(CursorOp::Seek(key)) => {
                self.inner.seek(key)?;
            }
            Some(CursorOp::SeekExact(key)) => {
                self.inner.seek_exact(key)?;
            }
            Some(CursorOp::Next(key)) => {
                self.inner.seek(key)?;
                self.inner.next()?;
            }
            None => {}
        }
        Ok(())
    }
}

impl<C: TrieCursor> TrieCursor for CachedTrieCursor<'_, C> {
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let op = CursorOp::SeekExact(key);
        if let Some(result) = self.memo.get(&op) {
            return Ok(result);
        }
        let result = self.inner.seek_exact(key)?;
        self.memo.insert(op, &result);
        Ok(result)
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let op = CursorOp::Seek(key);
        if let Some(result) = self.memo.get(&op) {
            return Ok(result);
        }
        let result = self.inner.seek(key)?;
        self.memo.insert(op, &result);
        Ok(result)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(key) = self.memo.position else {
            // Unpositioned `next` semantics are backend-specific, so read through uncached.
            self.sync()?;
            let result = self.inner.next()?;
            self.memo.position = result.as_ref().map(|(key, _)| *key);
            return Ok(result);
        };
        let op = CursorOp::Next(key);
        if let Some(result) = self.memo.get(&op) {
            return Ok(result);
        }
        // `key` is live, so seeking it lands the wrapped cursor exactly on the logical position.
        if self.memo.pending.is_some() {
            self.inner.seek(key)?;
        }
        let result = self.inner.next()?;
        self.memo.insert(op, &result);
        Ok(result)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        self.sync()?;
        self.inner.current()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.memo.unposition(self.memo.hashed_address);
    }
}

impl<C: TrieStorageCursor> TrieStorageCursor for CachedTrieCursor<'_, C> {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.inner.set_hashed_address(hashed_address);
        self.memo.unposition(Some(hashed_address));
    }
}

/// [`HashedCursorFactory`] whose cursors memoize results in a shared [`CursorResultCache`].
#[derive(Debug, Clone)]
pub struct CachedHashedCursorFactory<'c, F> {
    inner: F,
    cache: &'c CursorResultCache,
}

impl<'c, F> CachedHashedCursorFactory<'c, F> {
    /// Wraps `inner` so its cursors share `cache`.
    pub const fn new(inner: F, cache: &'c CursorResultCache) -> Self {
        Self { inner, cache }
    }
}

impl<F: HashedCursorFactory> HashedCursorFactory for CachedHashedCursorFactory<'_, F> {
    type AccountCursor<'a>
        = CachedHashedCursor<'a, F::AccountCursor<'a>, Account>
    where
        Self: 'a;
    type StorageCursor<'a>
        = CachedHashedCursor<'a, F::StorageCursor<'a>, U256>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(CachedHashedCursor {
            inner: self.inner.hashed_account_cursor()?,
            memo: CursorMemo::new(self.cache, &self.cache.hashed_accounts, None),
        })
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(CachedHashedCursor {
            inner: self.inner.hashed_storage_cursor(hashed_address)?,
            memo: CursorMemo::new(self.cache, &self.cache.hashed_storages, Some(hashed_address)),
        })
    }
}

/// [`HashedCursor`] that serves repeated operations from a [`CursorResultCache`].
#[derive(Debug)]
pub struct CachedHashedCursor<'c, C, V> {
    inner: C,
    memo: CursorMemo<'c, B256, V>,
}

impl<C, V> CachedHashedCursor<'_, C, V>
where
    C: HashedCursor<Value = V>,
    V: Clone,
{
    /// Applies the last cache-served operation to the wrapped cursor. Hashed cursors only issue
    /// [`CursorOp::Seek`] and [`CursorOp::Next`].
    fn sync(&mut self) -> Result<(), DatabaseError> {
        if let Some(op) = self.memo.pending.take() {
            let (CursorOp::Seek(key) | CursorOp::SeekExact(key) | CursorOp::Next(key)) = op;
            self.inner.seek(key)?;
            if matches!(op, CursorOp::Next(_)) {
                self.inner.next()?;
            }
        }
        Ok(())
    }
}

impl<C, V> HashedCursor for CachedHashedCursor<'_, C, V>
where
    C: HashedCursor<Value = V>,
    V: Clone + Debug,
{
    type Value = V;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, V)>, DatabaseError> {
        let op = CursorOp::Seek(key);
        if let Some(result) = self.memo.get(&op) {
            return Ok(result);
        }
        let result = self.inner.seek(key)?;
        self.memo.insert(op, &result);
        Ok(result)
    }

    fn next(&mut self) -> Result<Option<(B256, V)>, DatabaseError> {
        let Some(key) = self.memo.position else {
            // Unpositioned `next` semantics are backend-specific, so read through uncached.
            self.sync()?;
            let result = self.inner.next()?;
            self.memo.position = result.as_ref().map(|(key, _)| *key);
            return Ok(result);
        };
        let op = CursorOp::Next(key);
        if let Some(result) = self.memo.get(&op) {
            return Ok(result);
        }
        // `key` is live, so seeking it lands the wrapped cursor exactly on the logical position.
        if self.memo.pending.is_some() {
            self.inner.seek(key)?;
        }
        let result = self.inner.next()?;
        self.memo.insert(op, &result);
        Ok(result)
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.memo.unposition(self.memo.hashed_address);
    }
}

impl<C> HashedStorageCursor for CachedHashedCursor<'_, C, U256>
where
    C: HashedStorageCursor<Value = U256>,
{
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        let hashed_address = self.memo.hashed_address.unwrap_or_default();
        if let Some(empty) = self.memo.cache.empty_storages.get(&hashed_address) {
            self.memo.hits += 1;
            return Ok(*empty);
        }
        let empty = self.inner.is_storage_empty()?;
        // Some backends move the wrapped cursor here; re-seek the logical position before the
        // next read-through `next`.
        if let Some(key) = self.memo.position {
            self.memo.pending = Some(CursorOp::Seek(key));
        }
        self.memo.misses += 1;
        self.memo.cache.empty_storages.insert(hashed_address, empty);
        Ok(empty)
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.inner.set_hashed_address(hashed_address);
        self.memo.unposition(Some(hashed_address));
    }
}
