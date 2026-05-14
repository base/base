use std::{collections::BTreeMap, ops::RangeBounds, path::Path};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256, map::HashMap};
#[cfg(feature = "metrics")]
use eyre::WrapErr;
#[cfg(feature = "metrics")]
use metrics::{Label, gauge};
use reth_db::{
    Database, DatabaseEnv, DatabaseError,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    mdbx::{DatabaseArguments, init_db_for},
    table::{DupSort, Table},
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::Account;
use reth_trie::{hashed_cursor::HashedCursor, trie_cursor::TrieCursor};
use reth_trie_common::{
    BranchNodeCompact, HashedPostState, Nibbles, StoredNibbles,
    updates::{StorageTrieUpdates, TrieUpdates},
};
#[cfg(feature = "metrics")]
use tracing::error;

use super::{BlockNumberHash, ProofWindow, ProofWindowKey, Tables};
use crate::{
    BaseProofsStorageError,
    BaseProofsStorageError::NoBlocksFound,
    BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    api::{BaseProofsInitialStateStore, InitialStateAnchor, InitialStateStatus, WriteCounts},
    db::{
        MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
        cursor::Dup,
        models::{
            AccountTrieHistory, BlockChangeSet, ChangeSet, HashedAccountHistory,
            HashedStorageHistory, HashedStorageKey, IntoKV, MaybeDeleted, StorageTrieHistory,
            StorageTrieKey, StorageValue, VersionedValue,
        },
    },
};

/// MDBX implementation of [`BaseProofsStore`].
#[derive(Debug)]
pub struct MdbxProofsStorage {
    env: DatabaseEnv,
}

struct ProofWindowValue {
    earliest: NumHash,
    latest: NumHash,
}

/// Preprocessed prune plan for a target block number
#[derive(Debug, Clone)]
struct PrunePlan {
    earliest_block: u64,
    acc_survivors: Vec<(StoredNibbles, u64)>,
    storage_survivors: Vec<(StorageTrieKey, u64)>,
    hashed_acc_survivors: Vec<(B256, u64)>,
    hashed_storage_survivors: Vec<(HashedStorageKey, u64)>,
}

/// Preprocessed delete work for a prune range
#[derive(Debug, Default, Clone)]
struct HistoryDeleteBatch {
    account_trie: Vec<(<AccountTrieHistory as Table>::Key, u64)>,
    storage_trie: Vec<(<StorageTrieHistory as Table>::Key, u64)>,
    hashed_account: Vec<(<HashedAccountHistory as Table>::Key, u64)>,
    hashed_storage: Vec<(<HashedStorageHistory as Table>::Key, u64)>,
}

impl MdbxProofsStorage {
    /// Creates a new [`MdbxProofsStorage`] instance with the given path.
    pub fn new(path: &Path) -> Result<Self, BaseProofsStorageError> {
        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .map_err(|e| DatabaseError::Other(format!("Failed to open database: {e}")))?;
        Ok(Self { env })
    }

    fn inner_get_latest_block_number_hash(
        &self,
        tx: &impl DbTx,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let block = self.inner_get_block_number_hash(tx, ProofWindowKey::LatestBlock)?;
        if block.is_some() {
            return Ok(block);
        }

        self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock)
    }

    fn inner_get_block_number_hash(
        &self,
        tx: &impl DbTx,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let mut cursor = tx.cursor_read::<ProofWindow>()?;
        let value = cursor.seek_exact(key)?;
        Ok(value.map(|(_, val)| (val.number(), *val.hash())))
    }

    fn inner_get_proof_window(
        &self,
        tx: &impl DbTx,
    ) -> BaseProofsStorageResult<Option<ProofWindowValue>> {
        let mut cursor = tx.cursor_read::<ProofWindow>()?;

        let earliest = match cursor.seek_exact(ProofWindowKey::EarliestBlock)? {
            Some((_, val)) => NumHash::new(val.number(), *val.hash()),
            None => return Ok(None),
        };

        let latest = match cursor.seek_exact(ProofWindowKey::LatestBlock)? {
            Some((_, val)) => NumHash::new(val.number(), *val.hash()),
            None => earliest,
        };

        Ok(Some(ProofWindowValue { earliest, latest }))
    }

    fn set_earliest_block_number_hash(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let _ = self.env.update(|tx| {
            Self::inner_set_earliest_block_number(tx, block_number, hash)?;
            Ok::<(), DatabaseError>(())
        })?;
        Ok(())
    }

    /// Internal helper to set earliest block number hash within an existing transaction
    fn inner_set_earliest_block_number(
        tx: &(impl DbTxMut + DbTx),
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut cursor = tx.cursor_write::<ProofWindow>()?;
        cursor.upsert(ProofWindowKey::EarliestBlock, &BlockNumberHash::new(block_number, hash))?;
        Ok(())
    }

    /// Internal helper to set latest block number hash within an existing transaction
    fn inner_set_latest_block_number(
        tx: &(impl DbTxMut + DbTx),
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut cursor = tx.cursor_write::<ProofWindow>()?;
        cursor.upsert(ProofWindowKey::LatestBlock, &BlockNumberHash::new(block_number, hash))?;
        Ok(())
    }

    /// Persist a batch of versioned history entries to a dup-sorted table.
    ///
    /// # Parameters
    /// - `block_number`: Target block number for versioning entries
    /// - `items`: **Must be sorted** - iterator of entries to persist
    /// - `append_mode`: Mode selector for write strategy:
    ///   - `true` (Append): Appends all entries including tombstones for forward progress
    ///   - `false` (Prune): Removes tombstones, writes non-tombstones to block 0
    ///
    /// The cost of pruning is the cost of (append + deleting tombstones + deleting old block 0).
    /// The tombstones deletion is expensive as it requires a seek for each (key + subkey).
    ///
    /// Uses [`reth_db::mdbx::cursor::Cursor::upsert`] for upsert operation.
    fn persist_history_batch<T, I, V>(
        &self,
        tx: &(impl DbTxMut + DbTx),
        block_number: T::SubKey,
        items: I,
        append_mode: bool,
    ) -> BaseProofsStorageResult<Vec<T::Key>>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T::Key: Clone,
        I: IntoIterator,
        I::Item: IntoKV<T>,
    {
        let mut cur = tx.cursor_dup_write::<T>()?;
        let mut keys = Vec::<T::Key>::new();

        // Materialize iterator to enable partitioning and collect keys
        let mut pairs: Vec<(T::Key, T::Value)> = Vec::new();
        for it in items {
            let (k, vv) = it.into_kv(block_number);
            pairs.push((k.clone(), vv));
            keys.push(k)
        }

        if append_mode {
            // Append all entries (including tombstones) to preserve full history
            for (k, vv) in pairs {
                cur.append_dup(k.clone(), vv)?;
            }
            return Ok(keys);
        }

        // Drop current cursor to start clean for Phase 1
        drop(cur);

        // Phase 1: Batch Delete (Sequential)
        // Remove all existing state at Block 0 for these keys.
        {
            let mut del_cur = tx.cursor_dup_write::<T>()?;
            for (k, _) in &pairs {
                // Seek to (Key, Block 0)
                if let Some(vv) = del_cur.seek_by_key_subkey(k.clone(), 0)?
                    && vv.block_number == 0
                {
                    del_cur.delete_current()?;
                }
            }
        }

        // Phase 2: Batch Write (Sequential)
        // Write new values (skipping tombstones).
        {
            let mut write_cur = tx.cursor_dup_write::<T>()?;
            for (k, vv) in pairs {
                if vv.value.0.is_some() {
                    write_cur.upsert(k, &vv)?;
                }
            }
        }

        Ok(keys)
    }

    /// Delete entries for `items` at exactly `block_number` in a dup-sorted table.
    /// Seeks (key, block) and deletes current if the subkey matches.
    fn delete_dup_sorted<T, I, V>(
        &self,
        tx: &(impl DbTxMut + DbTx),
        items: I,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T::Key: Clone,
        T::SubKey: PartialEq + Clone,
        I: IntoIterator<Item = (T::Key, T::SubKey)>,
    {
        let mut cur = tx.cursor_dup_write::<T>()?;
        for (key, subkey) in items {
            if let Some(vv) = cur.seek_by_key_subkey(key, subkey)? {
                // ensure we didn't land on a >subkey
                if vv.block_number == subkey {
                    cur.delete_current()?;
                }
            }
        }
        Ok(())
    }

    /// Phase 1 of pruning: Calculate survivors.
    /// Scans change sets to find the LATEST update for every key in the range.
    fn calculate_prune_plan(
        &self,
        target_block: u64,
    ) -> BaseProofsStorageResult<Option<PrunePlan>> {
        self.env.view(|tx| {
            let Some((earliest, _)) =
                self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock)?
            else {
                return Ok(None);
            };

            if earliest >= target_block {
                return Ok(None);
            }

            // 1. Accumulate latest block per key using HashMap for O(1) deduplication
            // This is memory-efficient for high-churn scenarios (many updates to same keys).
            let mut acc_candidates: HashMap<StoredNibbles, u64> = HashMap::default();
            let mut storage_candidates: HashMap<StorageTrieKey, u64> = HashMap::default();
            let mut hashed_acc_candidates: HashMap<B256, u64> = HashMap::default();
            let mut hashed_storage_candidates: HashMap<HashedStorageKey, u64> = HashMap::default();

            let range = (earliest + 1)..=target_block;
            let mut cs_cursor = tx.cursor_read::<BlockChangeSet>()?;
            let mut walker = cs_cursor.walk_range(range)?;

            while let Some(Ok((block_number, cs))) = walker.next() {
                for k in cs.account_trie_keys {
                    acc_candidates
                        .entry(k)
                        .and_modify(|curr| *curr = (*curr).max(block_number))
                        .or_insert(block_number);
                }
                for k in cs.storage_trie_keys {
                    storage_candidates
                        .entry(k)
                        .and_modify(|curr| *curr = (*curr).max(block_number))
                        .or_insert(block_number);
                }
                for k in cs.hashed_account_keys {
                    hashed_acc_candidates
                        .entry(k)
                        .and_modify(|curr| *curr = (*curr).max(block_number))
                        .or_insert(block_number);
                }
                for k in cs.hashed_storage_keys {
                    hashed_storage_candidates
                        .entry(k)
                        .and_modify(|curr| *curr = (*curr).max(block_number))
                        .or_insert(block_number);
                }
            }

            // 2. Convert map to sorted survivors list for efficient sequential db write
            Ok(Some(PrunePlan {
                earliest_block: earliest,
                acc_survivors: Self::flatten_and_sort(acc_candidates),
                storage_survivors: Self::flatten_and_sort(storage_candidates),
                hashed_acc_survivors: Self::flatten_and_sort(hashed_acc_candidates),
                hashed_storage_survivors: Self::flatten_and_sort(hashed_storage_candidates),
            }))
        })?
    }

    /// Helper to flatten `HashMap` into a sorted Vector of survivors.
    /// Sorting is required to ensure optimal sequential seek performance in MDBX.
    fn flatten_and_sort<K: Ord>(map: HashMap<K, u64>) -> Vec<(K, u64)> {
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Delete history versions for `items` that are strictly older than the provided block number.
    /// `items` is a list of (Key, `SurvivorBlock`). Everything strictly older than `SurvivorBlock`
    /// is deleted. Returns the number of entries deleted.
    fn prune_history_preceding<T, V>(
        &self,
        tx: &(impl DbTxMut + DbTx),
        cutoff_items: Vec<(T::Key, u64)>,
    ) -> BaseProofsStorageResult<u64>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T::Key: Clone + Ord,
    {
        if cutoff_items.is_empty() {
            return Ok(0);
        }

        let mut deleted_count = 0;
        let mut cur = tx.cursor_dup_write::<T>()?;
        for (key, survivor_block) in cutoff_items {
            // Seek to the start of history for this key (Block 0)
            if let Some(mut entry) = cur.seek_by_key_subkey(key.clone(), 0)? {
                loop {
                    if entry.block_number >= survivor_block {
                        // Reached the survivor version (or newer). Stop deleting for this key.

                        // If the survivor is a tombstone (None), delete it too.
                        // Since we just deleted all older history, a tombstone at the start of
                        // history is redundant (it implies "does not
                        // exist").
                        if entry.block_number == survivor_block && entry.value.0.is_none() {
                            cur.delete_current()?;
                            deleted_count += 1;
                        }

                        break;
                    }

                    // Entry is strictly older than survivor. Delete it.
                    cur.delete_current()?;
                    deleted_count += 1;

                    // MDBX delete_current() automatically advances the cursor to the next item.
                    // We check if the next item is still the same key.
                    match cur.current() {
                        Ok(Some((k, v))) => {
                            if k != key {
                                break; // Moved past the key
                            }
                            entry = v;
                        }
                        _ => break, // End of table or error
                    }
                }
            }
        }
        Ok(deleted_count)
    }

    /// Tombstone every key returned by `next`, then overlay `new_entries` so collisions
    /// resolve in favor of `new_entries`, and `append_dup` the merged set in sorted-key order at
    /// `block_number`. Used by the wipe branches of `store_trie_updates_for_block` to keep new
    /// slots / nodes that arrive in the same block as a `wiped`/`is_deleted` entry —
    /// `HashedStorage::from_plain_storage` sets `wiped = was_destroyed()` (true for
    /// `DestroyedChanged`), so a SELFDESTRUCT + same-block recreate produces both at once.
    fn wipe_and_overlay<T, Next, I, K, VV, V>(
        &self,
        tx: &(impl DbTxMut + DbTx),
        block_number: u64,
        hashed_address: B256,
        mut next: Next,
        new_entries: I,
    ) -> BaseProofsStorageResult<Vec<T::Key>>
    where
        T: Table<Value = VersionedValue<V>> + DupSort,
        Next: FnMut() -> BaseProofsStorageResult<Option<(K, VV)>>,
        I: IntoIterator<Item = (K, Option<V>)>,
        (B256, K, Option<V>): IntoKV<T>,
        T::Key: Clone,
        K: Ord,
    {
        let mut merged: BTreeMap<K, Option<V>> = BTreeMap::new();
        while let Some((k, _vv)) = next()? {
            merged.insert(k, None);
        }
        for (k, v) in new_entries {
            merged.insert(k, v);
        }

        let mut cur = tx.cursor_dup_write::<T>()?;
        let mut keys: Vec<T::Key> = Vec::with_capacity(merged.len());
        for (k, value) in merged {
            let key: T::Key = (hashed_address, k, Option::<V>::None).into_key();
            let vv: T::Value = VersionedValue { block_number, value: MaybeDeleted(value) };
            cur.append_dup(key.clone(), vv)?;
            keys.push(key);
        }
        Ok(keys)
    }

    /// Collect versioned history over `block_range` using `BlockChangeSet`.
    fn collect_history_ranged(
        &self,
        tx: &impl DbTx,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<HistoryDeleteBatch> {
        let mut history = HistoryDeleteBatch::default();
        let mut change_set_cursor = tx.cursor_read::<BlockChangeSet>()?;
        let mut walker = change_set_cursor.walk_range(block_range)?;

        while let Some(Ok((block_number, change_set))) = walker.next() {
            // Push (key, subkey=block_number) pairs
            history
                .account_trie
                .extend(change_set.account_trie_keys.into_iter().map(|k| (k, block_number)));
            history
                .storage_trie
                .extend(change_set.storage_trie_keys.into_iter().map(|k| (k, block_number)));
            history
                .hashed_account
                .extend(change_set.hashed_account_keys.into_iter().map(|k| (k, block_number)));
            history
                .hashed_storage
                .extend(change_set.hashed_storage_keys.into_iter().map(|k| (k, block_number)));
        }

        // Sorting by tuple sorts by key first, then by block_number.
        history.account_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.storage_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_account.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_storage.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));

        Ok(history)
    }

    /// Delete versioned history over `block_range` using history batch.
    fn delete_history_ranged(
        &self,
        tx: &(impl DbTxMut + DbTx),
        block_range: impl RangeBounds<u64>,
        history: HistoryDeleteBatch,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let mut change_set_cursor = tx.cursor_write::<BlockChangeSet>()?;
        let mut walker = change_set_cursor.walk_range(block_range)?;

        while let Some(Ok((_, _))) = walker.next() {
            walker.delete_current()?;
        }

        // Delete using the simplified API: iterator of (key, subkey)
        self.delete_dup_sorted::<AccountTrieHistory, _, _>(tx, history.clone().account_trie)?;
        self.delete_dup_sorted::<StorageTrieHistory, _, _>(tx, history.clone().storage_trie)?;
        self.delete_dup_sorted::<HashedAccountHistory, _, _>(tx, history.clone().hashed_account)?;
        self.delete_dup_sorted::<HashedStorageHistory, _, _>(tx, history.clone().hashed_storage)?;

        Ok(WriteCounts {
            account_trie_updates_written_total: history.account_trie.len() as u64,
            storage_trie_updates_written_total: history.storage_trie.len() as u64,
            hashed_accounts_written_total: history.hashed_account.len() as u64,
            hashed_storages_written_total: history.hashed_storage.len() as u64,
        })
    }

    /// Write trie/state history for `block_number` from `block_state_diff`.
    fn store_trie_updates_for_block(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        block_number: u64,
        block_state_diff: BlockStateDiff,
        append_mode: bool,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;

        let storage_trie_len = sorted_trie_updates.storage_tries_ref().len();
        let hashed_storage_len = sorted_post_state.storages.len();

        let account_trie_keys = self.persist_history_batch(
            tx,
            block_number,
            sorted_trie_updates.account_nodes_ref().iter().cloned(),
            append_mode,
        )?;
        let hashed_account_keys = self.persist_history_batch(
            tx,
            block_number,
            sorted_post_state.accounts.iter().copied(),
            append_mode,
        )?;

        let mut storage_trie_keys = Vec::<StorageTrieKey>::with_capacity(storage_trie_len);
        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            if nodes.is_deleted && append_mode {
                let mut ro = self.storage_trie_cursor(*hashed_address, block_number - 1)?;
                let keys = self.wipe_and_overlay(
                    tx,
                    block_number,
                    *hashed_address,
                    || Ok(ro.next()?),
                    nodes.storage_nodes_ref().iter().cloned(),
                )?;
                storage_trie_keys.extend(keys);
                continue;
            }

            let keys = self.persist_history_batch(
                tx,
                block_number,
                nodes
                    .storage_nodes_ref()
                    .iter()
                    .cloned()
                    .map(|(path, node)| (*hashed_address, path, node)),
                append_mode,
            )?;
            storage_trie_keys.extend(keys);
        }

        let mut hashed_storage_keys = Vec::<HashedStorageKey>::with_capacity(hashed_storage_len);
        for (hashed_address, storage) in sorted_post_state.storages {
            if append_mode && storage.is_wiped() {
                let mut ro = self.storage_hashed_cursor(hashed_address, block_number - 1)?;
                let keys = self.wipe_and_overlay(
                    tx,
                    block_number,
                    hashed_address,
                    || Ok(ro.next()?),
                    storage
                        .storage_slots_ref()
                        .iter()
                        .map(|(slot, val)| (*slot, Some(StorageValue(*val)))),
                )?;
                hashed_storage_keys.extend(keys);
                continue;
            }
            let keys = self.persist_history_batch(
                tx,
                block_number,
                storage
                    .storage_slots_ref()
                    .iter()
                    .map(|(key, val)| (hashed_address, *key, Some(StorageValue(*val)))),
                append_mode,
            )?;
            hashed_storage_keys.extend(keys);
        }

        Ok(ChangeSet {
            account_trie_keys,
            storage_trie_keys,
            hashed_account_keys,
            hashed_storage_keys,
        })
    }

    /// Append-only writer for a block: validates parent, persists diff (soft-delete=true),
    /// records a `BlockChangeSet`, and advances `ProofWindow::LatestBlock`.
    fn store_trie_updates_append_only(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let block_number = block_ref.block.number;

        // Check the latest stored block is the parent of the incoming block
        let latest_block_hash =
            self.inner_get_latest_block_number_hash(tx)?.map_or(B256::ZERO, |(_num, hash)| hash);

        if latest_block_hash != block_ref.parent {
            return Err(BaseProofsStorageError::OutOfOrder {
                block_number,
                parent_block_hash: block_ref.parent,
                latest_block_hash,
            });
        }

        let change_set =
            &self.store_trie_updates_for_block(tx, block_number, block_state_diff, true)?;

        // Cursor for recording all changes made in this block for all history tables
        let mut change_set_cursor = tx.new_cursor::<BlockChangeSet>()?;
        change_set_cursor.append(block_number, change_set)?;

        // Update proof window's latest block
        Self::inner_set_latest_block_number(tx, block_number, block_ref.block.hash)?;

        Ok(WriteCounts {
            account_trie_updates_written_total: change_set.account_trie_keys.len() as u64,
            storage_trie_updates_written_total: change_set.storage_trie_keys.len() as u64,
            hashed_accounts_written_total: change_set.hashed_account_keys.len() as u64,
            hashed_storages_written_total: change_set.hashed_storage_keys.len() as u64,
        })
    }

    /// Return `BlockNumHash` for the initial state anchor.
    fn get_initial_state_anchor(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        self.env.view(|tx| {
            let mut cur = tx.cursor_read::<ProofWindow>()?;
            Ok(cur.seek_exact(ProofWindowKey::InitialStateAnchor)?.map(|(_k, v)| v.into()))
        })?
    }

    /// Return latest key for a table
    fn get_latest_key<T>(&self) -> BaseProofsStorageResult<Option<T::Key>>
    where
        T: Table,
    {
        self.env.view(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            Ok(cursor.last()?.map(|(k, _)| k))
        })?
    }
}

impl BaseProofsStore for MdbxProofsStorage {
    type StorageTrieCursor<'tx>
        = MdbxTrieCursor<StorageTrieHistory, Dup<'tx, StorageTrieHistory>>
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = MdbxTrieCursor<AccountTrieHistory, Dup<'tx, AccountTrieHistory>>
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = MdbxStorageCursor<Dup<'tx, HashedStorageHistory>>
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = MdbxAccountCursor<Dup<'tx, HashedAccountHistory>>
    where
        Self: 'tx;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.env.view(|tx| self.inner_get_block_number_hash(tx, ProofWindowKey::EarliestBlock))?
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.env.view(|tx| self.inner_get_latest_block_number_hash(tx))?
    }

    fn storage_trie_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        let tx = self.env.tx()?;
        let cursor = tx.cursor_dup_read::<StorageTrieHistory>()?;

        Ok(MdbxTrieCursor::new(cursor, max_block_number, Some(hashed_address)))
    }

    fn account_trie_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        let tx = self.env.tx()?;
        let cursor = tx.cursor_dup_read::<AccountTrieHistory>()?;

        Ok(MdbxTrieCursor::new(cursor, max_block_number, None))
    }

    fn storage_hashed_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        let tx = self.env.tx()?;
        let cursor = tx.cursor_dup_read::<HashedStorageHistory>()?;

        Ok(MdbxStorageCursor::new(cursor, max_block_number, hashed_address))
    }

    fn account_hashed_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        let tx = self.env.tx()?;
        let cursor = tx.cursor_dup_read::<HashedAccountHistory>()?;

        Ok(MdbxAccountCursor::new(cursor, max_block_number))
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        self.env
            .update(|tx| self.store_trie_updates_append_only(tx, block_ref, block_state_diff))?
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        self.env.view(|tx| {
            let mut change_set_cursor = tx.cursor_read::<BlockChangeSet>()?;
            let (_, change_set) = change_set_cursor
                .seek_exact(block_number)?
                .ok_or(BaseProofsStorageError::NoChangeSetForBlock(block_number))?;

            let mut account_trie_cursor = tx.new_cursor::<AccountTrieHistory>()?;
            let mut storage_trie_cursor = tx.new_cursor::<StorageTrieHistory>()?;
            let mut hashed_account_cursor = tx.new_cursor::<HashedAccountHistory>()?;
            let mut hashed_storage_cursor = tx.new_cursor::<HashedStorageHistory>()?;

            let mut trie_updates = TrieUpdates::default();
            for key in change_set.account_trie_keys {
                let entry =
                    match account_trie_cursor.seek_by_key_subkey(key.clone(), block_number)? {
                        Some(v) if v.block_number == block_number => v.value.0,
                        _ => {
                            return Err(BaseProofsStorageError::MissingAccountTrieHistory(
                                key.0,
                                block_number,
                            ));
                        }
                    };

                if let Some(value) = entry {
                    trie_updates.account_nodes.insert(key.0, value);
                } else {
                    trie_updates.removed_nodes.insert(key.0);
                }
            }

            for key in change_set.storage_trie_keys {
                let entry =
                    match storage_trie_cursor.seek_by_key_subkey(key.clone(), block_number)? {
                        Some(v) if v.block_number == block_number => v.value.0,
                        _ => {
                            return Err(BaseProofsStorageError::MissingStorageTrieHistory(
                                key.hashed_address,
                                key.path.0,
                                block_number,
                            ));
                        }
                    };

                let stu = trie_updates
                    .storage_tries
                    .entry(key.hashed_address)
                    .or_insert_with(StorageTrieUpdates::default);

                // handle is_deleted scenario
                // Issue: https://github.com/op-rs/op-reth/issues/323
                if let Some(value) = entry {
                    stu.storage_nodes.insert(key.path.0, value);
                } else {
                    stu.removed_nodes.insert(key.path.0);
                }
            }

            let mut post_state =
                HashedPostState::with_capacity(change_set.hashed_account_keys.len());
            for key in change_set.hashed_account_keys {
                let entry = match hashed_account_cursor.seek_by_key_subkey(key, block_number)? {
                    Some(v) if v.block_number == block_number => v.value.0,
                    _ => {
                        return Err(BaseProofsStorageError::MissingHashedAccountHistory(
                            key,
                            block_number,
                        ));
                    }
                };

                post_state.accounts.insert(key, entry);
            }

            for key in change_set.hashed_storage_keys {
                let entry =
                    match hashed_storage_cursor.seek_by_key_subkey(key.clone(), block_number)? {
                        Some(v) if v.block_number == block_number => v.value.0,
                        _ => {
                            return Err(BaseProofsStorageError::MissingHashedStorageHistory {
                                hashed_address: key.hashed_address,
                                hashed_storage_key: key.hashed_storage_key,
                                block_number,
                            });
                        }
                    };

                let hs = post_state.storages.entry(key.hashed_address).or_default();

                // handle wiped storage scenario
                // Issue: https://github.com/op-rs/op-reth/issues/323
                if let Some(value) = entry {
                    hs.storage.insert(key.hashed_storage_key, value.0);
                } else {
                    hs.storage.insert(key.hashed_storage_key, U256::ZERO);
                }
            }

            Ok(BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: post_state.into_sorted(),
            })
        })?
    }

    /// Update the initial state with the provided diff.
    /// Prune all historical trie data till `new_earliest_block_number` (inclusive) using
    /// the [`BlockChangeSet`] index.
    ///
    /// Arguments:
    /// - `new_earliest_block_ref`: The new earliest block reference (with parent hash).
    /// - `diff`: The state diff to apply to the initial state (block 0). This diff represents all
    ///   the changes from the old earliest block to the new earliest block (inclusive).
    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let target_block = new_earliest_block_ref.block.number;

        // --- PHASE 1: READ (Calculate Deletions) ---
        let plan = self.calculate_prune_plan(target_block)?;
        let Some(plan) = plan else {
            return Ok(WriteCounts::default());
        };

        // --- PHASE 2: WRITE (Execute Deletions) ---
        self.env.update(|tx| {
            // 1. Execute Sparse Deletions and track actual deleted rows
            let acc_deleted =
                self.prune_history_preceding::<AccountTrieHistory, _>(tx, plan.acc_survivors)?;

            let st_deleted =
                self.prune_history_preceding::<StorageTrieHistory, _>(tx, plan.storage_survivors)?;

            let ha_deleted = self.prune_history_preceding::<HashedAccountHistory, _>(
                tx,
                plan.hashed_acc_survivors,
            )?;

            let hs_deleted = self.prune_history_preceding::<HashedStorageHistory, _>(
                tx,
                plan.hashed_storage_survivors,
            )?;

            let counts = WriteCounts {
                account_trie_updates_written_total: acc_deleted,
                storage_trie_updates_written_total: st_deleted,
                hashed_accounts_written_total: ha_deleted,
                hashed_storages_written_total: hs_deleted,
            };

            // 2. Delete ChangeSets
            let range = (plan.earliest_block + 1)..=target_block;
            let mut cs_cursor = tx.cursor_write::<BlockChangeSet>()?;
            let mut walker = cs_cursor.walk_range(range)?;
            while walker.next().is_some() {
                walker.delete_current()?;
            }

            // 3. Update Earliest Pointer
            Self::inner_set_earliest_block_number(
                tx,
                target_block,
                new_earliest_block_ref.block.hash,
            )?;

            Ok(counts)
        })?
    }

    /// Unwind the historical state to `unwind_upto_block` (inclusive), deleting all history
    /// starting from provided block. Also updates the `ProofWindow::LatestBlock` to parent of
    /// `unwind_upto_block`.
    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        let history_to_delete =
            self.env.view(|tx| self.collect_history_ranged(tx, to.block.number..))??;

        self.env.update(|tx| {
            let proof_window = match self.inner_get_proof_window(tx)? {
                Some(pw) => pw,
                None => return Ok(()), // Nothing to unwind
            };

            if to.block.number > proof_window.latest.number {
                return Ok(()); // Nothing to unwind
            }

            if to.block.number <= proof_window.earliest.number {
                return Err(BaseProofsStorageError::UnwindBeyondEarliest {
                    unwind_block_number: to.block.number,
                    earliest_block_number: proof_window.earliest.number,
                });
            }

            self.delete_history_ranged(tx, to.block.number.., history_to_delete)?;

            let new_latest_block =
                BlockNumberHash::new(to.block.number.saturating_sub(1), to.parent);

            // Update proof window's Latest block
            Self::inner_set_latest_block_number(
                tx,
                new_latest_block.number(),
                *new_latest_block.hash(),
            )?;

            Ok(())
        })?
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        mut blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        // Sort the vec list by block number
        blocks_to_add.sort_unstable_by_key(|(bwp, _)| bwp.block.number);

        let history_to_delete = self
            .env
            .view(|tx| self.collect_history_ranged(tx, latest_common_block.number + 1..))??;

        self.env.update(|tx| {
            // Remove the old history
            self.delete_history_ranged(tx, latest_common_block.number + 1.., history_to_delete)?;

            // Update the ProofWindow Latest Block to latest_common_block so we can perform
            // `store_trie_updates_append_only`.
            Self::inner_set_latest_block_number(
                tx,
                latest_common_block.number,
                latest_common_block.hash,
            )?;

            // Apply the new history
            for (block_with_parent, diff) in blocks_to_add {
                self.store_trie_updates_append_only(tx, block_with_parent, diff)?;
            }
            Ok(())
        })?
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.set_earliest_block_number_hash(block_number, hash)
    }
}

impl BaseProofsInitialStateStore for MdbxProofsStorage {
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        // 1) NotStarted: no anchor row
        let Some(block) = self.get_initial_state_anchor()? else {
            return Ok(InitialStateAnchor::default());
        };

        // 2) Completed: anchor exists + earliest is set
        let completed = self.get_earliest_block_number()?.is_some();

        // 3) InProgress / Completed: populate details
        Ok(InitialStateAnchor {
            block: Some(block),
            status: if completed {
                InitialStateStatus::Completed
            } else {
                InitialStateStatus::InProgress
            },
            latest_account_trie_key: self.get_latest_key::<AccountTrieHistory>()?,
            latest_storage_trie_key: self.get_latest_key::<StorageTrieHistory>()?,
            latest_hashed_account_key: self.get_latest_key::<HashedAccountHistory>()?,
            latest_hashed_storage_key: self.get_latest_key::<HashedStorageHistory>()?,
        })
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        self.env.update(|tx| {
            let mut cur = tx.cursor_write::<ProofWindow>()?;
            cur.insert(ProofWindowKey::InitialStateAnchor, &anchor.into())?;
            Ok(())
        })?
    }

    fn store_account_branches(
        &self,
        account_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut account_nodes = account_nodes;
        if account_nodes.is_empty() {
            return Ok(());
        }

        account_nodes.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            self.persist_history_batch(tx, 0, account_nodes.into_iter(), true)?;
            Ok(())
        })?
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut storage_nodes = storage_nodes;
        if storage_nodes.is_empty() {
            return Ok(());
        }

        storage_nodes.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            self.persist_history_batch(
                tx,
                0,
                storage_nodes.into_iter().map(|(path, node)| (hashed_address, path, node)),
                true,
            )?;
            Ok(())
        })?
    }

    fn store_hashed_accounts(
        &self,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut accounts = accounts;
        if accounts.is_empty() {
            return Ok(());
        }

        // sort the accounts by key to ensure insertion is efficient
        accounts.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            self.persist_history_batch(tx, 0, accounts.into_iter(), true)?;
            Ok(())
        })?
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        let mut storages = storages;
        if storages.is_empty() {
            return Ok(());
        }

        // sort the storages by key to ensure insertion is efficient
        storages.sort_by_key(|(key, _)| *key);

        self.env.update(|tx| {
            self.persist_history_batch(
                tx,
                0,
                storages
                    .into_iter()
                    .map(|(key, val)| (hashed_address, key, Some(StorageValue(val)))),
                true,
            )?;
            Ok(())
        })?
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        self.set_earliest_block_number(anchor.number, anchor.hash)?;
        Ok(anchor)
    }
}

/// This implementation is copied from the
/// [`DatabaseMetrics`](reth_db::database_metrics::DatabaseMetrics) implementation for
/// [`DatabaseEnv`]. As the implementation hard-coded the table name, we need to reimplement it.
#[cfg(feature = "metrics")]
impl reth_db::database_metrics::DatabaseMetrics for MdbxProofsStorage {
    fn report_metrics(&self) {
        for (name, value, labels) in self.gauge_metrics() {
            gauge!(name, labels).set(value);
        }
    }

    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        let mut metrics = Vec::new();

        let _ = self
            .env
            .view(|tx| {
                for table in Tables::ALL.iter().map(Tables::name) {
                    let table_db =
                        tx.inner().open_db(Some(table)).wrap_err("Could not open db.")?;

                    let stats = tx
                        .inner()
                        .db_stat(table_db.dbi())
                        .wrap_err(format!("Could not find table: {table}"))?;

                    let page_size = stats.page_size() as usize;
                    let leaf_pages = stats.leaf_pages();
                    let branch_pages = stats.branch_pages();
                    let overflow_pages = stats.overflow_pages();
                    let num_pages = leaf_pages + branch_pages + overflow_pages;
                    let table_size = page_size * num_pages;
                    let entries = stats.entries();

                    metrics.push((
                        "base_proof_storage.table_size",
                        table_size as f64,
                        vec![Label::new("table", table)],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        leaf_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "leaf")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        branch_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "branch")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_pages",
                        overflow_pages as f64,
                        vec![Label::new("table", table), Label::new("type", "overflow")],
                    ));
                    metrics.push((
                        "base_proof_storage.table_entries",
                        entries as f64,
                        vec![Label::new("table", table)],
                    ));
                }

                Ok::<(), eyre::Report>(())
            })
            .map_err(|error| error!(%error, "Failed to read db table stats"));

        if let Ok(freelist) =
            self.env.freelist().map_err(|error| error!(%error, "Failed to read db.freelist"))
        {
            metrics.push(("base_proof_storage.freelist", freelist as f64, vec![]));
        }

        if let Ok(stat) = self.env.stat().map_err(|error| error!(%error, "Failed to read db.stat"))
        {
            metrics.push(("base_proof_storage.page_size", stat.page_size() as f64, vec![]));
        }

        metrics.push((
            "base_proof_storage.timed_out_not_aborted_transactions",
            self.env.timed_out_not_aborted_transactions() as f64,
            vec![],
        ));

        metrics
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::NumHash;
    use alloy_primitives::B256;
    use reth_db::{
        DatabaseError,
        cursor::DbDupCursorRO,
        transaction::{DbTx, DbTxMut},
    };
    use reth_trie::{
        BranchNodeCompact, HashedPostStateSorted, HashedStorage, Nibbles, StoredNibbles,
        updates::{StorageTrieUpdates, TrieUpdatesSorted},
    };
    use tempfile::TempDir;

    use super::*;
    use crate::db::{
        StorageTrieKey,
        models::{AccountTrieHistory, StorageTrieHistory},
    };

    const B0: u64 = 0;

    #[test]
    fn test_store_trie_updates_comprehensive() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // Sample block number
        const BLOCK: BlockWithParent =
            BlockWithParent::new(B256::ZERO, NumHash::new(42, B256::ZERO));

        // Sample addresses and keys
        let addr1 = B256::from([0x11; 32]);
        let addr2 = B256::from([0x22; 32]);
        let slot1 = B256::from([0xA1; 32]);
        let slot2 = B256::from([0xA2; 32]);

        // Sample accounts
        let acc1 = Account { nonce: 1, balance: U256::from(100), ..Default::default() };

        // Sample storage values
        let val1 = U256::from(1234u64);
        let val2 = U256::from(5678u64);

        // Sample trie paths
        let account_path1 = Nibbles::from_nibbles_unchecked(vec![0, 1, 2, 3]);
        let account_path2 = Nibbles::from_nibbles_unchecked(vec![4, 5, 6, 7]);
        let removed_account_path = Nibbles::from_nibbles_unchecked(vec![7, 8, 9]);

        let account_node1 = BranchNodeCompact::default();
        let account_node2 = BranchNodeCompact::default();

        let storage_path1 = Nibbles::from_nibbles_unchecked(vec![1, 2, 3, 4]);
        let storage_path2 = Nibbles::from_nibbles_unchecked(vec![8, 9, 0, 1]);

        let storage_node1 = BranchNodeCompact::default();
        let storage_node2 = BranchNodeCompact::default();

        // Construct test BlockStateDiff
        let mut block_state_diff_trie_updates = TrieUpdates::default();
        let mut block_state_diff_post_state = HashedPostState::default();

        // Add account trie nodes
        block_state_diff_trie_updates.account_nodes.insert(account_path1, account_node1);
        block_state_diff_trie_updates.account_nodes.insert(account_path2, account_node2);
        block_state_diff_trie_updates.removed_nodes.insert(removed_account_path);

        // Add storage trie nodes for two addresses
        let mut storage_nodes1 = StorageTrieUpdates::default();
        storage_nodes1.storage_nodes.insert(storage_path1, storage_node1);
        block_state_diff_trie_updates.storage_tries.insert(addr1, storage_nodes1);

        let mut storage_nodes2 = StorageTrieUpdates::default();
        storage_nodes2.storage_nodes.insert(storage_path2, storage_node2);
        block_state_diff_trie_updates.storage_tries.insert(addr2, storage_nodes2);

        // Add hashed accounts (one Some, one None)
        block_state_diff_post_state.accounts.insert(addr1, Some(acc1));
        block_state_diff_post_state.accounts.insert(addr2, None); // Deletion

        // Add storage slots for both addresses
        let mut storage1 = HashedStorage::default();
        storage1.storage.insert(slot1, val1);
        block_state_diff_post_state.storages.insert(addr1, storage1);

        let mut storage2 = HashedStorage::default();
        storage2.storage.insert(slot2, val2);
        block_state_diff_post_state.storages.insert(addr2, storage2);

        // Store everything
        let block_state_diff = BlockStateDiff {
            sorted_trie_updates: block_state_diff_trie_updates.into_sorted(),
            sorted_post_state: block_state_diff_post_state.into_sorted(),
        };
        store.store_trie_updates(BLOCK, block_state_diff).expect("store");

        // Verify account trie nodes
        {
            let tx = store.env.tx().expect("tx");
            let mut cur = tx.new_cursor::<AccountTrieHistory>().expect("cursor");

            // Check first node
            let vv1 = cur
                .seek_by_key_subkey(account_path1.into(), BLOCK.block.number)
                .expect("seek")
                .expect("exists");
            assert_eq!(vv1.block_number, BLOCK.block.number);
            assert!(vv1.value.0.is_some());

            // Check second node
            let vv2 = cur
                .seek_by_key_subkey(account_path2.into(), BLOCK.block.number)
                .expect("seek")
                .expect("exists");
            assert_eq!(vv2.block_number, BLOCK.block.number);
            assert!(vv2.value.0.is_some());

            // Check removed node
            let vv3 = cur
                .seek_by_key_subkey(removed_account_path.into(), BLOCK.block.number)
                .expect("seek")
                .expect("exists");
            assert_eq!(vv3.block_number, BLOCK.block.number);
            assert!(vv3.value.0.is_none(), "Expected node deletion");
        }

        // Verify storage trie nodes
        {
            let tx = store.env.tx().expect("tx");
            let mut cur = tx.new_cursor::<StorageTrieHistory>().expect("cursor");

            // Check node for addr1
            let key1 = StorageTrieKey::new(addr1, storage_path1.into());
            let vv1 =
                cur.seek_by_key_subkey(key1, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv1.block_number, BLOCK.block.number);
            assert!(vv1.value.0.is_some());

            // Check node for addr2
            let key2 = StorageTrieKey::new(addr2, storage_path2.into());
            let vv2 =
                cur.seek_by_key_subkey(key2, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv2.block_number, BLOCK.block.number);
            assert!(vv2.value.0.is_some());
        }

        // Verify hashed accounts
        {
            let tx = store.env.tx().expect("tx");
            let mut cur = tx.new_cursor::<HashedAccountHistory>().expect("cursor");

            // Check account1 (exists)
            let vv1 =
                cur.seek_by_key_subkey(addr1, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv1.block_number, BLOCK.block.number);
            assert_eq!(vv1.value.0, Some(acc1));

            // Check account2 (deletion)
            let vv2 =
                cur.seek_by_key_subkey(addr2, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv2.block_number, BLOCK.block.number);
            assert!(vv2.value.0.is_none(), "Expected account deletion");
        }

        // Verify hashed storages
        {
            let tx = store.env.tx().expect("tx");
            let mut cur = tx.new_cursor::<HashedStorageHistory>().expect("cursor");

            // Check storage for addr1
            let key1 = HashedStorageKey::new(addr1, slot1);
            let vv1 =
                cur.seek_by_key_subkey(key1, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv1.block_number, BLOCK.block.number);
            let inner1 = vv1.value.0.as_ref().expect("Some(StorageValue)");
            assert_eq!(inner1.0, val1);

            // Check storage for addr2
            let key2 = HashedStorageKey::new(addr2, slot2);
            let vv2 =
                cur.seek_by_key_subkey(key2, BLOCK.block.number).expect("seek").expect("exists");
            assert_eq!(vv2.block_number, BLOCK.block.number);
            let inner2 = vv2.value.0.as_ref().expect("Some(StorageValue)");
            assert_eq!(inner2.0, val2);
        }

        // Verify BlockChangeSet entries
        {
            let tx = store.env.tx().expect("tx");
            let mut cur = tx.new_cursor::<BlockChangeSet>().expect("cursor");
            let entries: Vec<_> = cur.walk(Some(BLOCK.block.number)).expect("walk").collect();
            assert_eq!(entries.len(), 1, "Expected 1 BlockChangeSet entry");
        }

        // check the latest block number in proof window
        {
            let tx = store.env.tx().expect("tx");
            let mut proof_window_cursor = tx.new_cursor::<ProofWindow>().expect("cursor");
            let latest_block = proof_window_cursor
                .seek(ProofWindowKey::LatestBlock)
                .expect("seek")
                .expect("exists");
            assert_eq!(latest_block.1.number(), BLOCK.block.number);
            assert_eq!(*latest_block.1.hash(), BLOCK.block.hash);
        }
    }

    #[test]
    fn test_store_trie_updates_empty_collections() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        const BLOCK: BlockWithParent =
            BlockWithParent::new(B256::ZERO, NumHash::new(42, B256::ZERO));

        // Create BlockStateDiff with empty collections
        let block_state_diff = BlockStateDiff::default();

        // This should work without errors
        store.store_trie_updates(BLOCK, block_state_diff).expect("store");

        // Verify nothing was written (should be empty)
        let tx = store.env.tx().expect("tx");

        let mut cur1 = tx.new_cursor::<AccountTrieHistory>().expect("cursor");
        assert!(cur1.next_dup_val().expect("first").is_none(), "Account trie should be empty");

        let mut cur2 = tx.new_cursor::<StorageTrieHistory>().expect("cursor");
        assert!(cur2.next_dup_val().expect("first").is_none(), "Storage trie should be empty");

        let mut cur3 = tx.new_cursor::<HashedAccountHistory>().expect("cursor");
        assert!(cur3.next_dup_val().expect("first").is_none(), "Hashed accounts should be empty");

        let mut cur4 = tx.new_cursor::<HashedStorageHistory>().expect("cursor");
        assert!(cur4.next_dup_val().expect("first").is_none(), "Hashed storage should be empty");

        let mut cur5 = tx.new_cursor::<BlockChangeSet>().expect("cursor");
        assert!(
            cur5.next().expect("first").is_some(),
            "Pruning index SHOULD populate the change set even for empty diffs"
        );
    }

    #[test]
    fn fetch_trie_updates_missing_account_history_entry_returns_error() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // prepare ChangeSet that references StoredNibbles for account key
        // (insert ChangeSet into BlockChangeSet directly using tx)
        {
            let tx = store.env.tx_mut().unwrap();
            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    account_trie_keys: vec![StoredNibbles::default()],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingAccountTrieHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_account_history_seek_returns_later_block_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // manually insert account history and ChangeSet for block 1 referencing same key
        {
            let tx = store.env.tx_mut().unwrap();
            let mut acc_cur = tx.cursor_write::<AccountTrieHistory>().unwrap();
            acc_cur
                .insert(
                    StoredNibbles::from(Nibbles::from_nibbles_unchecked([0x1])),
                    &VersionedValue::new(2, MaybeDeleted(Some(BranchNodeCompact::default()))),
                )
                .unwrap();

            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    account_trie_keys: vec![StoredNibbles::from(Nibbles::from_nibbles_unchecked(
                        [0x1],
                    ))],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // fetch block 1 -> seek will find block 2 but block_number != 1 so expect
        // MissingAccountTrieHistory
        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingAccountTrieHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_missing_storage_history_entry_returns_error() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // prepare ChangeSet that references StorageTrieKey for storage trie
        // (insert ChangeSet into BlockChangeSet directly using tx)
        {
            let tx = store.env.tx_mut().unwrap();
            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    storage_trie_keys: vec![StorageTrieKey::new(
                        B256::from([0u8; 32]),
                        StoredNibbles::default(),
                    )],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingStorageTrieHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_storage_history_seek_returns_later_block_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // manually insert storage history and ChangeSet for block 1 referencing same key
        {
            let tx = store.env.tx_mut().unwrap();
            let mut stor_cur = tx.cursor_write::<StorageTrieHistory>().unwrap();
            stor_cur
                .insert(
                    StorageTrieKey::new(
                        B256::from([0u8; 32]),
                        StoredNibbles::from(Nibbles::from_nibbles_unchecked([0x1])),
                    ),
                    &VersionedValue::new(2, MaybeDeleted(Some(BranchNodeCompact::default()))),
                )
                .unwrap();

            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    storage_trie_keys: vec![StorageTrieKey::new(
                        B256::from([0u8; 32]),
                        StoredNibbles::from(Nibbles::from_nibbles_unchecked([0x1])),
                    )],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // fetch block 1 -> seek will find block 2 but block_number != 1 so expect
        // MissingStorageTrieHistory
        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingStorageTrieHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_missing_hashed_account_entry_returns_error() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // prepare ChangeSet that references hashed account address
        // (insert ChangeSet into BlockChangeSet directly using tx)
        {
            let tx = store.env.tx_mut().unwrap();
            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    hashed_account_keys: vec![B256::from([0u8; 32])],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingHashedAccountHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_hashed_account_seek_returns_later_block_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // manually insert hashed account history and ChangeSet for block 1 referencing same key
        {
            let tx = store.env.tx_mut().unwrap();
            let mut acc_cur = tx.cursor_write::<HashedAccountHistory>().unwrap();
            acc_cur
                .insert(
                    B256::from([0u8; 32]),
                    &VersionedValue::new(2, MaybeDeleted(Some(Account::default()))),
                )
                .unwrap();

            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    hashed_account_keys: vec![B256::from([0u8; 32])],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // fetch block 1 -> seek will find block 2 but block_number != 1 so expect
        // MissingHashedAccountHistory
        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingHashedAccountHistory(..))));
    }

    #[test]
    fn fetch_trie_updates_missing_hashed_storage_entry_returns_error() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // prepare ChangeSet that references hashed storage key
        // (insert ChangeSet into BlockChangeSet directly using tx)
        {
            let tx = store.env.tx_mut().unwrap();
            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    hashed_storage_keys: vec![HashedStorageKey::new(
                        B256::from([0u8; 32]),
                        B256::from([0u8; 32]),
                    )],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingHashedStorageHistory { .. })));
    }

    #[test]
    fn fetch_trie_updates_hashed_storage_seek_returns_later_block_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");

        // manually insert hashed storage history and ChangeSet for block 1 referencing same key
        {
            let tx = store.env.tx_mut().unwrap();
            let mut stor_cur = tx.cursor_write::<HashedStorageHistory>().unwrap();
            stor_cur
                .insert(
                    HashedStorageKey::new(B256::from([0u8; 32]), B256::from([0u8; 32])),
                    &VersionedValue::new(2, MaybeDeleted(Some(StorageValue::new(U256::ZERO)))),
                )
                .unwrap();

            let mut cur = tx.cursor_write::<BlockChangeSet>().unwrap();
            cur.insert(
                1,
                &ChangeSet {
                    hashed_storage_keys: vec![HashedStorageKey::new(
                        B256::from([0u8; 32]),
                        B256::from([0u8; 32]),
                    )],
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // fetch block 1 -> seek will find block 2 but block_number != 1 so expect
        // MissingHashedStorageHistory
        let res = store.fetch_trie_updates(1);
        assert!(matches!(res, Err(BaseProofsStorageError::MissingHashedStorageHistory { .. })));
    }

    #[test]
    fn test_prune_earliest_state_with_removed_nodes() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        store.set_earliest_block_number(0, B256::ZERO).unwrap();

        // Create some trie nodes in blocks 1, 2, 3
        let path1 = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
        let path2 = Nibbles::from_nibbles_unchecked([0x03, 0x04]);
        let node1 = BranchNodeCompact::new(0b1, 0, 0, vec![], Some(B256::random()));
        let node2 = BranchNodeCompact::new(0b10, 0, 0, vec![], Some(B256::random()));

        let block_1 = BlockWithParent::new(B256::ZERO, NumHash::new(1, B256::random()));
        let mut diff1_trie_updates = TrieUpdates::default();
        diff1_trie_updates.account_nodes.insert(path1, node1);
        let diff1 = BlockStateDiff {
            sorted_trie_updates: diff1_trie_updates.into_sorted(),
            ..Default::default()
        };
        store.store_trie_updates(block_1, diff1).unwrap();

        let block_2 = BlockWithParent::new(block_1.block.hash, NumHash::new(2, B256::random()));
        let mut diff2_trie_updates = TrieUpdates::default();
        diff2_trie_updates.account_nodes.insert(path2, node2.clone());
        let diff2 = BlockStateDiff {
            sorted_trie_updates: diff2_trie_updates.into_sorted(),
            ..Default::default()
        };
        store.store_trie_updates(block_2, diff2).unwrap();

        // In block 3, path1 is deleted (stored as None in the database)
        // This happens when we store trie updates with path1 mapped to None
        let block_3 = BlockWithParent::new(block_2.block.hash, NumHash::new(3, B256::random()));
        // Simulate storing a deletion by directly writing to DB
        store
            .env
            .update(|tx| {
                let mut cursor = tx.new_cursor::<AccountTrieHistory>()?;
                let vv = VersionedValue { block_number: 3, value: MaybeDeleted(None) };
                cursor.upsert(StoredNibbles::from(path1), &vv)?;

                // Record in change set
                let mut change_set_cursor = tx.new_cursor::<BlockChangeSet>()?;
                change_set_cursor.upsert(
                    3,
                    &ChangeSet {
                        account_trie_keys: vec![StoredNibbles::from(path1)],
                        storage_trie_keys: vec![],
                        hashed_account_keys: vec![],
                        hashed_storage_keys: vec![],
                    },
                )?;

                // Update proof window
                let mut proof_window_cursor = tx.new_cursor::<ProofWindow>()?;
                proof_window_cursor.upsert(
                    ProofWindowKey::LatestBlock,
                    &BlockNumberHash::new(3, block_3.block.hash),
                )?;
                Ok::<(), DatabaseError>(())
            })
            .unwrap()
            .unwrap();

        // Now prune to block 5, with the new initial state:
        // - path1 should be in removed_nodes (it was deleted in block 3)
        // - path2 should be included with its value (it still exists from block 2)
        let block_5 = BlockWithParent::new(B256::random(), NumHash::new(5, B256::random()));
        store.prune_earliest_state(block_5).unwrap();

        // Verify that all entries for path1 before block 5 were removed
        let tx = store.env.tx().unwrap();
        let mut cur = tx.cursor_dup_read::<AccountTrieHistory>().unwrap();

        // path1 at block 1 should be gone; seeking 1 finds survivor (tombstone at 3)
        if let Some(v) = cur.seek_by_key_subkey(StoredNibbles::from(path1), 1).unwrap() {
            assert!(v.block_number >= 3, "path1 at block 1 should be pruned");
        }

        // path1 at block 1 should be gone.
        // path1 survivor at block 3 (tombstone) should ALSO be gone now (optimization).
        // So seeking for path1 should return None.
        assert!(
            cur.seek_by_key_subkey(StoredNibbles::from(path1), 0).unwrap().is_none(),
            "path1 should be completely removed including tombstone"
        );

        // path2 entries should be pruned (blocks < 5)
        // Survivor for path2 is at block 2.
        let v2 =
            cur.seek_by_key_subkey(StoredNibbles::from(path2), 0).unwrap().expect("path2 survivor");
        assert_eq!(v2.block_number, 2);
        assert_eq!(v2.value.0, Some(node2));
    }

    #[test]
    fn test_block_change_set_crud_operations() {
        let dir = TempDir::new().unwrap();
        let store = MdbxProofsStorage::new(dir.path()).expect("env");
        let tx = store.env.tx_mut().expect("rw tx");
        let mut cursor = tx.cursor_write::<BlockChangeSet>().expect("cursor");

        let block_1 = 42u64;
        let block_2 = 43u64;

        let entry1 = ChangeSet {
            account_trie_keys: vec![StoredNibbles::default()],
            storage_trie_keys: vec![],
            hashed_account_keys: vec![B256::ZERO],
            hashed_storage_keys: vec![],
        };
        let entry2 = ChangeSet {
            account_trie_keys: vec![],
            storage_trie_keys: vec![StorageTrieKey::new(B256::ZERO, StoredNibbles::default())],
            hashed_account_keys: vec![],
            hashed_storage_keys: vec![HashedStorageKey::new(B256::ZERO, B256::ZERO)],
        };

        // Insert entries
        cursor.insert(block_1, &entry1).unwrap();
        cursor.insert(block_2, &entry2).unwrap();

        // Read entries
        let mut walker = cursor.walk(Some(block_1)).unwrap();
        let mut entries = vec![walker.next().unwrap().unwrap().1];
        if let Some(Ok((_, val))) = walker.next() {
            entries.push(val);
        }
        entries.sort();
        let mut expected = vec![entry1.clone(), entry2.clone()];
        expected.sort();
        assert_eq!(entries, expected);

        // Delete entry1
        let mut walker = cursor.walk(Some(block_1)).unwrap();
        while let Some(Ok((_, val))) = walker.next() {
            if val == entry1 {
                walker.delete_current().unwrap();
                break;
            }
        }

        // Verify delete
        let mut walker = cursor.walk(Some(block_1)).unwrap();
        assert_eq!(walker.next().unwrap().unwrap().1, entry2);
        assert!(walker.next().is_none());
    }

}
