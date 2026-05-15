//! `RocksDB` implementation of proofs storage.

use std::{
    collections::BTreeMap,
    fmt,
    marker::PhantomData,
    ops::{Bound, RangeBounds},
    path::Path,
    sync::Arc,
};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256, map::HashMap};
#[cfg(feature = "metrics")]
use metrics::Label;
use parking_lot::Mutex;
use reth_db::{
    DatabaseError,
    table::{Compress, Decompress, DupSort, Encode, Table},
};
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{
    BranchNodeCompact, HashedPostState, Nibbles, StoredNibbles,
    updates::{StorageTrieUpdates, TrieUpdates},
};
use rocksdb::{
    BoundColumnFamily, ColumnFamilyDescriptor, DBCompressionType, DBIteratorWithThreadMode,
    DBWithThreadMode, Direction, IteratorMode, MultiThreaded, Options, SnapshotWithThreadMode,
    WriteBatch, WriteOptions,
};
use tracing::trace;

use super::{BlockNumberHash, ProofWindow, ProofWindowKey};
use crate::{
    BaseProofsStorageError,
    BaseProofsStorageError::NoBlocksFound,
    BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    api::{BaseProofsInitialStateStore, InitialStateAnchor, InitialStateStatus, WriteCounts},
    db::{
        AccountTrieHistory, BlockChangeSet, ChangeSet, HashedAccountHistory, HashedStorageHistory,
        HashedStorageKey, IntoKV, MaybeDeleted, StorageTrieHistory, StorageTrieKey, StorageValue,
        VersionedValue,
    },
};

type RocksDb = DBWithThreadMode<MultiThreaded>;
type RocksDbLatestVersionResult<T> =
    Result<Option<(<T as Table>::Key, <T as Table>::Value)>, DatabaseError>;

const HASH_KEY_LEN: usize = 32;
const PACKED_NIBBLES_KEY_LEN: usize = 33;
const BLOCK_NUMBER_KEY_LEN: usize = 8;

trait RocksDbHistoryTable: Table + DupSort<SubKey = u64> {
    /// Fixed encoded table-key length before the block-number suffix.
    const KEY_LEN: usize;

    /// Encodes the table key prefix used before the block-number suffix.
    fn encode_history_key_prefix(key: &Self::Key) -> Vec<u8>;

    /// Decodes the table key prefix used before the block-number suffix.
    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError>;
}

/// `RocksDB` implementation of [`BaseProofsStore`].
pub struct RocksdbProofsStorage {
    db: Arc<RocksDb>,
    write_options: WriteOptions,
    write_lock: Mutex<()>,
}

/// Preprocessed prune plan for a target block number.
#[derive(Debug, Clone)]
struct RocksdbPrunePlan {
    earliest_block: u64,
    earliest_hash: B256,
    acc_survivors: Vec<(StoredNibbles, u64)>,
    storage_survivors: Vec<(StorageTrieKey, u64)>,
    hashed_acc_survivors: Vec<(B256, u64)>,
    hashed_storage_survivors: Vec<(HashedStorageKey, u64)>,
}

/// Preprocessed delete work for a prune commit.
#[derive(Debug, Clone)]
struct RocksdbPreparedPrune {
    expected_earliest_block: u64,
    expected_earliest_hash: B256,
    target_block: u64,
    deletes: RocksdbPreparedHistoryDeletes,
    counts: WriteCounts,
}

/// Raw history keys to delete during a prune commit.
#[derive(Debug, Default, Clone)]
struct RocksdbPreparedHistoryDeletes {
    account_trie: Vec<Vec<u8>>,
    storage_trie: Vec<Vec<u8>>,
    hashed_account: Vec<Vec<u8>>,
    hashed_storage: Vec<Vec<u8>>,
}

/// Preprocessed delete work for a prune range.
#[derive(Debug, Default)]
struct RocksdbHistoryDeleteBatch {
    block_numbers: Vec<u64>,
    account_trie: Vec<(<AccountTrieHistory as Table>::Key, u64)>,
    storage_trie: Vec<(<StorageTrieHistory as Table>::Key, u64)>,
    hashed_account: Vec<(<HashedAccountHistory as Table>::Key, u64)>,
    hashed_storage: Vec<(<HashedStorageHistory as Table>::Key, u64)>,
}

/// Request-scoped read snapshot for [`RocksdbProofsStorage`].
///
/// This type is public because it is the [`BaseProofsStore::Tx`] associated type for the
/// `RocksDB` backend. Callers that need several cursors to read the same database view should
/// acquire one snapshot with [`BaseProofsStore::ro_tx`] and pass it to the `*_with_tx` cursor
/// factories.
pub struct RocksdbReadSnapshot<'db> {
    db: &'db RocksDb,
    snapshot: SnapshotWithThreadMode<'db, RocksDb>,
}

/// Cursor over `RocksDB` versioned history rows.
struct RocksdbVersionedCursor<'db, T: Table + DupSort> {
    snapshot: Arc<RocksdbReadSnapshot<'db>>,
    max_block_number: u64,
    current_key: Option<T::Key>,
    _table: PhantomData<T>,
}

/// `RocksDB` implementation of [`TrieCursor`].
pub struct RocksdbTrieCursor<'db, T: Table + DupSort> {
    inner: RocksdbVersionedCursor<'db, T>,
    hashed_address: Option<B256>,
}

/// `RocksDB` implementation of [`HashedCursor`] for storage state.
pub struct RocksdbStorageCursor<'db> {
    inner: RocksdbVersionedCursor<'db, HashedStorageHistory>,
    hashed_address: B256,
}

/// `RocksDB` implementation of [`HashedCursor`] for account state.
pub struct RocksdbAccountCursor<'db> {
    inner: RocksdbVersionedCursor<'db, HashedAccountHistory>,
}

#[derive(Debug, Default)]
struct RocksdbReplacementState {
    storage_trie: BTreeMap<StorageTrieKey, Option<BranchNodeCompact>>,
    hashed_storage: BTreeMap<HashedStorageKey, Option<StorageValue>>,
}

#[derive(Debug, Clone, Copy)]
struct ProofWindowValue {
    earliest: NumHash,
    latest: NumHash,
}

impl fmt::Debug for RocksdbProofsStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbProofsStorage").finish_non_exhaustive()
    }
}

impl<'db> RocksdbReadSnapshot<'db> {
    fn new(db: &'db RocksDb) -> Self {
        Self::assert_send_sync();

        let snapshot = db.snapshot();
        Self { db, snapshot }
    }

    const fn assert_send_sync()
    where
        Self: Send + Sync,
    {
    }

    fn cf(&self, name: &'static str) -> Result<Arc<BoundColumnFamily<'_>>, DatabaseError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| DatabaseError::Other(format!("missing RocksDB column family {name}")))
    }
}

impl fmt::Debug for RocksdbReadSnapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbReadSnapshot").finish_non_exhaustive()
    }
}

impl<T> fmt::Debug for RocksdbVersionedCursor<'_, T>
where
    T: Table + DupSort,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbVersionedCursor")
            .field("max_block_number", &self.max_block_number)
            .finish_non_exhaustive()
    }
}

impl<T> fmt::Debug for RocksdbTrieCursor<'_, T>
where
    T: Table + DupSort,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbTrieCursor")
            .field("hashed_address", &self.hashed_address)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RocksdbStorageCursor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbStorageCursor")
            .field("hashed_address", &self.hashed_address)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RocksdbAccountCursor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbAccountCursor").finish_non_exhaustive()
    }
}

impl RocksdbReplacementState {
    fn storage_trie_wipe_entries(
        &self,
        storage: &RocksdbProofsStorage,
        base_block_number: u64,
        hashed_address: B256,
    ) -> BaseProofsStorageResult<BTreeMap<Nibbles, Option<BranchNodeCompact>>> {
        let mut entries = BTreeMap::new();
        let mut cursor = storage.storage_trie_cursor(hashed_address, base_block_number)?;

        while let Some((path, _)) = cursor.next()? {
            entries.insert(path, None);
        }

        for (key, value) in &self.storage_trie {
            if key.hashed_address != hashed_address {
                continue;
            }

            let path = key.path.0;
            if value.is_some() {
                entries.insert(path, None);
            } else {
                entries.remove(&path);
            }
        }

        Ok(entries)
    }

    fn apply_storage_trie_entries(
        &mut self,
        hashed_address: B256,
        entries: impl IntoIterator<Item = (Nibbles, Option<BranchNodeCompact>)>,
    ) {
        for (path, node) in entries {
            self.storage_trie
                .insert(StorageTrieKey::new(hashed_address, StoredNibbles::from(path)), node);
        }
    }

    fn hashed_storage_wipe_entries(
        &self,
        storage: &RocksdbProofsStorage,
        base_block_number: u64,
        hashed_address: B256,
    ) -> BaseProofsStorageResult<BTreeMap<B256, Option<StorageValue>>> {
        let mut entries = BTreeMap::new();
        let mut cursor = storage.storage_hashed_cursor(hashed_address, base_block_number)?;

        while let Some((slot, _)) = cursor.next()? {
            entries.insert(slot, None);
        }

        for (key, value) in &self.hashed_storage {
            if key.hashed_address != hashed_address {
                continue;
            }

            if let Some(value) = value
                && !value.0.is_zero()
            {
                entries.insert(key.hashed_storage_key, None);
            } else {
                entries.remove(&key.hashed_storage_key);
            }
        }

        Ok(entries)
    }

    fn apply_hashed_storage_entries(
        &mut self,
        hashed_address: B256,
        entries: impl IntoIterator<Item = (B256, Option<StorageValue>)>,
    ) {
        for (hashed_storage_key, value) in entries {
            self.hashed_storage
                .insert(HashedStorageKey::new(hashed_address, hashed_storage_key), value);
        }
    }
}

impl RocksdbProofsStorage {
    /// Creates a new [`RocksdbProofsStorage`] instance with the given path.
    pub fn new(path: &Path) -> Result<Self, BaseProofsStorageError> {
        let mut db_options = Options::default();
        db_options.create_if_missing(true);
        db_options.create_missing_column_families(true);
        db_options.set_max_background_jobs(8);

        let cf_options = Self::cf_options();
        let descriptors = Self::column_families()
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, cf_options.clone()));
        let db = RocksDb::open_cf_descriptors(&db_options, path, descriptors)
            .map_err(|e| DatabaseError::Other(format!("failed to open RocksDB database: {e}")))?;

        let mut write_options = WriteOptions::default();
        // Proof history is derivable from the canonical chain, so use async WAL writes for
        // throughput. RocksDB write batches still keep committed updates internally consistent.
        write_options.set_sync(false);

        Ok(Self { db: Arc::new(db), write_options, write_lock: Mutex::new(()) })
    }

    fn cf_options() -> Options {
        let mut options = Options::default();
        options.set_compression_type(DBCompressionType::None);
        options.set_level_compaction_dynamic_level_bytes(true);
        options.set_max_write_buffer_number(6);
        options.set_target_file_size_base(256 * 1024 * 1024);
        options.set_write_buffer_size(256 * 1024 * 1024);
        options
    }

    const fn column_families() -> [&'static str; 6] {
        [
            <AccountTrieHistory as Table>::NAME,
            <StorageTrieHistory as Table>::NAME,
            <HashedAccountHistory as Table>::NAME,
            <HashedStorageHistory as Table>::NAME,
            <ProofWindow as Table>::NAME,
            <BlockChangeSet as Table>::NAME,
        ]
    }

    fn cf(&self, name: &'static str) -> BaseProofsStorageResult<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| DatabaseError::Other(format!("missing RocksDB column family {name}")))
            .map_err(Into::into)
    }

    fn put_table<T: Table>(
        &self,
        batch: &mut WriteBatch,
        key: T::Key,
        value: &T::Value,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(T::NAME)?;
        batch.put_cf(&cf, encode_table_key::<T>(key), encode_table_value::<T>(value));
        Ok(())
    }

    fn get_table<T: Table>(&self, key: T::Key) -> BaseProofsStorageResult<Option<T::Value>> {
        let cf = self.cf(T::NAME)?;
        self.db
            .get_cf(&cf, encode_table_key::<T>(key))
            .map_err(rocksdb_error)?
            .map(|value| T::Value::decompress(&value).map_err(Into::into))
            .transpose()
    }

    fn get_table_from_snapshot<T: Table>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: T::Key,
    ) -> BaseProofsStorageResult<Option<T::Value>> {
        let cf = self.cf(T::NAME)?;
        snapshot
            .get_cf(&cf, encode_table_key::<T>(key))
            .map_err(rocksdb_error)?
            .map(|value| T::Value::decompress(&value).map_err(Into::into))
            .transpose()
    }

    fn persist_history_batch<T, I, V>(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        items: I,
        append_mode: bool,
    ) -> BaseProofsStorageResult<Vec<T::Key>>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T: RocksDbHistoryTable,
        T::Key: Clone,
        I: IntoIterator,
        I::Item: IntoKV<T>,
    {
        let cf = self.cf(T::NAME)?;
        let mut keys = Vec::<T::Key>::new();
        let mut pairs = Vec::<(T::Key, T::Value)>::new();

        for item in items {
            let (key, value) = item.into_kv(block_number);
            keys.push(key.clone());
            pairs.push((key, value));
        }

        if append_mode {
            for (key, value) in pairs {
                batch.put_cf(
                    &cf,
                    encode_history_key::<T>(&key, value.block_number),
                    encode_table_value::<T>(&value),
                );
            }
            return Ok(keys);
        }

        for (key, value) in pairs {
            batch.delete_cf(&cf, encode_history_key::<T>(&key, 0));
            if value.value.0.is_some() {
                batch.put_cf(
                    &cf,
                    encode_history_key::<T>(&key, 0),
                    encode_table_value::<T>(&value),
                );
            }
        }

        Ok(keys)
    }

    fn delete_dup_sorted<T, I, V>(
        &self,
        batch: &mut WriteBatch,
        items: I,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T: RocksDbHistoryTable,
        T::Key: Clone,
        I: IntoIterator<Item = (T::Key, u64)>,
    {
        let cf = self.cf(T::NAME)?;
        for (key, block_number) in items {
            batch.delete_cf(&cf, encode_history_key::<T>(&key, block_number));
        }
        Ok(())
    }

    fn collect_history_preceding_deletes<T, V>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        cutoff_items: Vec<(T::Key, u64)>,
    ) -> BaseProofsStorageResult<Vec<Vec<u8>>>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T: RocksDbHistoryTable,
        T::Key: Clone + Ord,
        T::Value: Decompress,
    {
        if cutoff_items.is_empty() {
            return Ok(Vec::new());
        }

        let cf = self.cf(T::NAME)?;
        let mut deletes = Vec::new();

        for (key, survivor_block) in cutoff_items {
            let prefix = encode_history_key_prefix::<T>(&key);
            let start_key = encode_history_key::<T>(&key, 0);
            let iter =
                snapshot.iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));

            for item in iter {
                let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
                if !raw_key.starts_with(&prefix) {
                    break;
                }

                let (_, block_number) = decode_history_key::<T>(&raw_key)?;
                if block_number >= survivor_block {
                    let value = T::Value::decompress(&raw_value)?;
                    if block_number == survivor_block && value.value.0.is_none() {
                        deletes.push(raw_key.to_vec());
                    }
                    break;
                }

                deletes.push(raw_key.to_vec());
            }
        }

        Ok(deletes)
    }

    fn delete_raw_history_keys<T>(
        &self,
        batch: &mut WriteBatch,
        keys: Vec<Vec<u8>>,
    ) -> BaseProofsStorageResult<()>
    where
        T: Table,
    {
        let cf = self.cf(T::NAME)?;
        for key in keys {
            batch.delete_cf(&cf, key);
        }
        Ok(())
    }

    fn wipe_and_overlay<T, Next, I, K, VV, V>(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        hashed_address: B256,
        mut next: Next,
        new_entries: I,
    ) -> BaseProofsStorageResult<Vec<T::Key>>
    where
        T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
        T: RocksDbHistoryTable,
        Next: FnMut() -> BaseProofsStorageResult<Option<(K, VV)>>,
        I: IntoIterator<Item = (K, Option<V>)>,
        (B256, K, Option<V>): IntoKV<T>,
        T::Key: Clone,
        K: Ord,
    {
        let cf = self.cf(T::NAME)?;
        let mut merged: BTreeMap<K, Option<V>> = BTreeMap::new();
        while let Some((key, _)) = next()? {
            merged.insert(key, None);
        }
        for (key, value) in new_entries {
            merged.insert(key, value);
        }

        let mut keys = Vec::with_capacity(merged.len());
        for (key, value) in merged {
            let db_key: T::Key = (hashed_address, key, Option::<V>::None).into_key();
            let db_value: T::Value = VersionedValue { block_number, value: MaybeDeleted(value) };
            batch.put_cf(
                &cf,
                encode_history_key::<T>(&db_key, block_number),
                encode_table_value::<T>(&db_value),
            );
            keys.push(db_key);
        }

        Ok(keys)
    }

    fn store_trie_updates_for_block(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        block_state_diff: BlockStateDiff,
        append_mode: bool,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;

        let storage_trie_len = sorted_trie_updates.storage_tries_ref().len();
        let hashed_storage_len = sorted_post_state.storages.len();

        let account_trie_keys = self.persist_history_batch::<AccountTrieHistory, _, _>(
            batch,
            block_number,
            sorted_trie_updates.account_nodes_ref().iter().cloned(),
            append_mode,
        )?;
        let hashed_account_keys = self.persist_history_batch::<HashedAccountHistory, _, _>(
            batch,
            block_number,
            sorted_post_state.accounts.iter().copied(),
            append_mode,
        )?;

        let mut storage_trie_keys = Vec::with_capacity(storage_trie_len);
        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            if nodes.is_deleted && append_mode {
                let mut cursor =
                    self.storage_trie_cursor(*hashed_address, block_number.saturating_sub(1))?;
                let keys = self.wipe_and_overlay::<StorageTrieHistory, _, _, _, _, _>(
                    batch,
                    block_number,
                    *hashed_address,
                    || Ok(cursor.next()?),
                    nodes.storage_nodes_ref().iter().cloned(),
                )?;
                storage_trie_keys.extend(keys);
                continue;
            }

            let keys = self.persist_history_batch::<StorageTrieHistory, _, _>(
                batch,
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

        let mut hashed_storage_keys = Vec::with_capacity(hashed_storage_len);
        for (hashed_address, storage) in sorted_post_state.storages {
            if append_mode && storage.is_wiped() {
                let mut cursor =
                    self.storage_hashed_cursor(hashed_address, block_number.saturating_sub(1))?;
                let keys = self.wipe_and_overlay::<HashedStorageHistory, _, _, _, _, _>(
                    batch,
                    block_number,
                    hashed_address,
                    || Ok(cursor.next()?),
                    storage
                        .storage_slots_ref()
                        .iter()
                        .map(|(slot, value)| (*slot, Some(StorageValue(*value)))),
                )?;
                hashed_storage_keys.extend(keys);
                continue;
            }

            let keys = self.persist_history_batch::<HashedStorageHistory, _, _>(
                batch,
                block_number,
                storage
                    .storage_slots_ref()
                    .iter()
                    .map(|(key, value)| (hashed_address, *key, Some(StorageValue(*value)))),
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

    fn store_trie_updates_append_only(
        &self,
        batch: &mut WriteBatch,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let block_number = block_ref.block.number;
        // This DB read intentionally assumes `batch` has no pending `LatestBlock` update. RocksDB
        // reads do not observe uncommitted `WriteBatch` entries.
        let latest_block_hash =
            self.get_latest_block_number_hash()?.map_or(B256::ZERO, |(_, hash)| hash);

        if latest_block_hash != block_ref.parent {
            return Err(BaseProofsStorageError::OutOfOrder {
                block_number,
                parent_block_hash: block_ref.parent,
                latest_block_hash,
            });
        }

        let change_set =
            self.store_trie_updates_for_block(batch, block_number, block_state_diff, true)?;
        self.put_table::<BlockChangeSet>(batch, block_number, &change_set)?;
        self.put_proof_window(
            batch,
            ProofWindowKey::LatestBlock,
            block_number,
            block_ref.block.hash,
        )?;

        Ok(WriteCounts {
            account_trie_updates_written_total: change_set.account_trie_keys.len() as u64,
            storage_trie_updates_written_total: change_set.storage_trie_keys.len() as u64,
            hashed_accounts_written_total: change_set.hashed_account_keys.len() as u64,
            hashed_storages_written_total: change_set.hashed_storage_keys.len() as u64,
        })
    }

    fn store_replacement_trie_updates_append_only(
        &self,
        batch: &mut WriteBatch,
        base_block_number: u64,
        replacement_state: &mut RocksdbReplacementState,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let block_number = block_ref.block.number;
        let change_set = self.store_replacement_trie_updates_for_block(
            batch,
            base_block_number,
            replacement_state,
            block_number,
            block_state_diff,
        )?;

        self.put_table::<BlockChangeSet>(batch, block_number, &change_set)?;
        self.put_proof_window(
            batch,
            ProofWindowKey::LatestBlock,
            block_number,
            block_ref.block.hash,
        )?;

        Ok(WriteCounts {
            account_trie_updates_written_total: change_set.account_trie_keys.len() as u64,
            storage_trie_updates_written_total: change_set.storage_trie_keys.len() as u64,
            hashed_accounts_written_total: change_set.hashed_account_keys.len() as u64,
            hashed_storages_written_total: change_set.hashed_storage_keys.len() as u64,
        })
    }

    fn store_replacement_trie_updates_for_block(
        &self,
        batch: &mut WriteBatch,
        base_block_number: u64,
        replacement_state: &mut RocksdbReplacementState,
        block_number: u64,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;

        let storage_trie_len = sorted_trie_updates.storage_tries_ref().len();
        let hashed_storage_len = sorted_post_state.storages.len();

        let account_trie_keys = self.persist_history_batch::<AccountTrieHistory, _, _>(
            batch,
            block_number,
            sorted_trie_updates.account_nodes_ref().iter().cloned(),
            true,
        )?;
        let hashed_account_keys = self.persist_history_batch::<HashedAccountHistory, _, _>(
            batch,
            block_number,
            sorted_post_state.accounts.iter().copied(),
            true,
        )?;

        let mut storage_trie_keys = Vec::with_capacity(storage_trie_len);
        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            let storage_entries = if nodes.is_deleted {
                let mut entries = replacement_state.storage_trie_wipe_entries(
                    self,
                    base_block_number,
                    *hashed_address,
                )?;
                for (path, node) in nodes.storage_nodes_ref().iter().cloned() {
                    entries.insert(path, node);
                }
                entries.into_iter().collect::<Vec<_>>()
            } else {
                nodes.storage_nodes_ref().to_vec()
            };

            let keys = self.persist_history_batch::<StorageTrieHistory, _, _>(
                batch,
                block_number,
                storage_entries.iter().cloned().map(|(path, node)| (*hashed_address, path, node)),
                true,
            )?;
            replacement_state.apply_storage_trie_entries(*hashed_address, storage_entries);
            storage_trie_keys.extend(keys);
        }

        let mut hashed_storage_keys = Vec::with_capacity(hashed_storage_len);
        for (hashed_address, storage) in sorted_post_state.storages {
            let storage_entries = if storage.is_wiped() {
                let mut entries = replacement_state.hashed_storage_wipe_entries(
                    self,
                    base_block_number,
                    hashed_address,
                )?;
                for (slot, value) in storage.storage_slots_ref() {
                    entries.insert(*slot, Some(StorageValue(*value)));
                }
                entries.into_iter().collect::<Vec<_>>()
            } else {
                storage
                    .storage_slots_ref()
                    .iter()
                    .map(|(key, value)| (*key, Some(StorageValue(*value))))
                    .collect::<Vec<_>>()
            };

            let keys = self.persist_history_batch::<HashedStorageHistory, _, _>(
                batch,
                block_number,
                storage_entries.iter().map(|(key, value)| (hashed_address, *key, *value)),
                true,
            )?;
            replacement_state.apply_hashed_storage_entries(hashed_address, storage_entries);
            hashed_storage_keys.extend(keys);
        }

        Ok(ChangeSet {
            account_trie_keys,
            storage_trie_keys,
            hashed_account_keys,
            hashed_storage_keys,
        })
    }

    fn get_block_number_hash(
        &self,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        Ok(self.get_table::<ProofWindow>(key)?.map(|value| (value.number(), *value.hash())))
    }

    fn get_block_number_hash_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        Ok(self
            .get_table_from_snapshot::<ProofWindow>(snapshot, key)?
            .map(|value| (value.number(), *value.hash())))
    }

    fn get_latest_block_number_hash(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let block = self.get_block_number_hash(ProofWindowKey::LatestBlock)?;
        if block.is_some() {
            return Ok(block);
        }

        self.get_block_number_hash(ProofWindowKey::EarliestBlock)
    }

    fn get_proof_window(&self) -> BaseProofsStorageResult<Option<ProofWindowValue>> {
        let Some((earliest_number, earliest_hash)) =
            self.get_block_number_hash(ProofWindowKey::EarliestBlock)?
        else {
            return Ok(None);
        };

        let latest = self.get_block_number_hash(ProofWindowKey::LatestBlock)?.map_or_else(
            || NumHash::new(earliest_number, earliest_hash),
            |(number, hash)| NumHash::new(number, hash),
        );

        Ok(Some(ProofWindowValue {
            earliest: NumHash::new(earliest_number, earliest_hash),
            latest,
        }))
    }

    fn put_proof_window(
        &self,
        batch: &mut WriteBatch,
        key: ProofWindowKey,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.put_table::<ProofWindow>(batch, key, &BlockNumberHash::new(block_number, hash))
    }

    fn set_earliest_block_number_hash(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        self.put_proof_window(&mut batch, ProofWindowKey::EarliestBlock, block_number, hash)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn calculate_prune_plan(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        target_block: u64,
    ) -> BaseProofsStorageResult<Option<RocksdbPrunePlan>> {
        let Some((earliest, earliest_hash)) =
            self.get_block_number_hash_from_snapshot(snapshot, ProofWindowKey::EarliestBlock)?
        else {
            return Ok(None);
        };

        if earliest >= target_block {
            return Ok(None);
        }

        let mut acc_candidates: HashMap<StoredNibbles, u64> = HashMap::default();
        let mut storage_candidates: HashMap<StorageTrieKey, u64> = HashMap::default();
        let mut hashed_acc_candidates: HashMap<B256, u64> = HashMap::default();
        let mut hashed_storage_candidates: HashMap<HashedStorageKey, u64> = HashMap::default();

        for (block_number, change_set) in
            self.iter_change_sets_from_snapshot(snapshot, (earliest + 1)..=target_block)?
        {
            for key in change_set.account_trie_keys {
                acc_candidates
                    .entry(key)
                    .and_modify(|current| *current = (*current).max(block_number))
                    .or_insert(block_number);
            }
            for key in change_set.storage_trie_keys {
                storage_candidates
                    .entry(key)
                    .and_modify(|current| *current = (*current).max(block_number))
                    .or_insert(block_number);
            }
            for key in change_set.hashed_account_keys {
                hashed_acc_candidates
                    .entry(key)
                    .and_modify(|current| *current = (*current).max(block_number))
                    .or_insert(block_number);
            }
            for key in change_set.hashed_storage_keys {
                hashed_storage_candidates
                    .entry(key)
                    .and_modify(|current| *current = (*current).max(block_number))
                    .or_insert(block_number);
            }
        }

        Ok(Some(RocksdbPrunePlan {
            earliest_block: earliest,
            earliest_hash,
            acc_survivors: flatten_and_sort(acc_candidates),
            storage_survivors: flatten_and_sort(storage_candidates),
            hashed_acc_survivors: flatten_and_sort(hashed_acc_candidates),
            hashed_storage_survivors: flatten_and_sort(hashed_storage_candidates),
        }))
    }

    fn collect_history_ranged(
        &self,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<RocksdbHistoryDeleteBatch> {
        let mut history = RocksdbHistoryDeleteBatch::default();

        for (block_number, change_set) in self.iter_change_sets(block_range)? {
            history.block_numbers.push(block_number);
            history
                .account_trie
                .extend(change_set.account_trie_keys.into_iter().map(|key| (key, block_number)));
            history
                .storage_trie
                .extend(change_set.storage_trie_keys.into_iter().map(|key| (key, block_number)));
            history
                .hashed_account
                .extend(change_set.hashed_account_keys.into_iter().map(|key| (key, block_number)));
            history
                .hashed_storage
                .extend(change_set.hashed_storage_keys.into_iter().map(|key| (key, block_number)));
        }

        history.account_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.storage_trie.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_account.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));
        history.hashed_storage.sort_by(|(k1, b1), (k2, b2)| k1.cmp(k2).then_with(|| b1.cmp(b2)));

        Ok(history)
    }

    fn delete_history_ranged(
        &self,
        batch: &mut WriteBatch,
        history: RocksdbHistoryDeleteBatch,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let cf = self.cf(<BlockChangeSet as Table>::NAME)?;
        for block_number in &history.block_numbers {
            batch.delete_cf(&cf, encode_block_number(*block_number));
        }

        let RocksdbHistoryDeleteBatch {
            block_numbers: _,
            account_trie,
            storage_trie,
            hashed_account,
            hashed_storage,
        } = history;
        let counts = WriteCounts {
            account_trie_updates_written_total: account_trie.len() as u64,
            storage_trie_updates_written_total: storage_trie.len() as u64,
            hashed_accounts_written_total: hashed_account.len() as u64,
            hashed_storages_written_total: hashed_storage.len() as u64,
        };

        self.delete_dup_sorted::<AccountTrieHistory, _, _>(batch, account_trie)?;
        self.delete_dup_sorted::<StorageTrieHistory, _, _>(batch, storage_trie)?;
        self.delete_dup_sorted::<HashedAccountHistory, _, _>(batch, hashed_account)?;
        self.delete_dup_sorted::<HashedStorageHistory, _, _>(batch, hashed_storage)?;

        Ok(counts)
    }

    fn iter_change_sets(
        &self,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<Vec<(u64, ChangeSet)>> {
        let cf = self.cf(<BlockChangeSet as Table>::NAME)?;
        let start = range_start(&block_range);
        let start_key = encode_block_number(start);
        let iter = self.db.iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));
        let mut rows = Vec::new();

        for item in iter {
            let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
            let block_number = decode_block_number(&raw_key)?;
            if !block_range.contains(&block_number) {
                break;
            }
            rows.push((block_number, ChangeSet::decompress(&raw_value)?));
        }

        Ok(rows)
    }

    fn iter_change_sets_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<Vec<(u64, ChangeSet)>> {
        let cf = self.cf(<BlockChangeSet as Table>::NAME)?;
        let start = range_start(&block_range);
        let start_key = encode_block_number(start);
        let iter = snapshot.iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));
        let mut rows = Vec::new();

        for item in iter {
            let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
            let block_number = decode_block_number(&raw_key)?;
            if !block_range.contains(&block_number) {
                break;
            }
            rows.push((block_number, ChangeSet::decompress(&raw_value)?));
        }

        Ok(rows)
    }

    fn prepare_prune(
        &self,
        target_block: u64,
    ) -> BaseProofsStorageResult<Option<RocksdbPreparedPrune>> {
        let snapshot = self.db.snapshot();
        let Some(plan) = self.calculate_prune_plan(&snapshot, target_block)? else {
            return Ok(None);
        };

        let account_trie = self.collect_history_preceding_deletes::<AccountTrieHistory, _>(
            &snapshot,
            plan.acc_survivors,
        )?;
        let storage_trie = self.collect_history_preceding_deletes::<StorageTrieHistory, _>(
            &snapshot,
            plan.storage_survivors,
        )?;
        let hashed_account = self.collect_history_preceding_deletes::<HashedAccountHistory, _>(
            &snapshot,
            plan.hashed_acc_survivors,
        )?;
        let hashed_storage = self.collect_history_preceding_deletes::<HashedStorageHistory, _>(
            &snapshot,
            plan.hashed_storage_survivors,
        )?;

        let counts = WriteCounts {
            account_trie_updates_written_total: account_trie.len() as u64,
            storage_trie_updates_written_total: storage_trie.len() as u64,
            hashed_accounts_written_total: hashed_account.len() as u64,
            hashed_storages_written_total: hashed_storage.len() as u64,
        };

        Ok(Some(RocksdbPreparedPrune {
            expected_earliest_block: plan.earliest_block,
            expected_earliest_hash: plan.earliest_hash,
            target_block,
            deletes: RocksdbPreparedHistoryDeletes {
                account_trie,
                storage_trie,
                hashed_account,
                hashed_storage,
            },
            counts,
        }))
    }

    fn commit_prepared_prune(
        &self,
        prepared: RocksdbPreparedPrune,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let _guard = self.write_lock.lock();

        let current_earliest = self.get_block_number_hash(ProofWindowKey::EarliestBlock)?;
        let expected_earliest =
            Some((prepared.expected_earliest_block, prepared.expected_earliest_hash));
        if current_earliest != expected_earliest {
            trace!(
                target: "trie::pruner",
                current_earliest = ?current_earliest,
                expected_earliest = ?expected_earliest,
                target_block = prepared.target_block,
                "skipping stale prune plan"
            );
            return Ok(WriteCounts::default());
        }

        let RocksdbPreparedPrune { expected_earliest_block, target_block, deletes, counts, .. } =
            prepared;
        let mut batch = WriteBatch::default();

        self.delete_raw_history_keys::<AccountTrieHistory>(&mut batch, deletes.account_trie)?;
        self.delete_raw_history_keys::<StorageTrieHistory>(&mut batch, deletes.storage_trie)?;
        self.delete_raw_history_keys::<HashedAccountHistory>(&mut batch, deletes.hashed_account)?;
        self.delete_raw_history_keys::<HashedStorageHistory>(&mut batch, deletes.hashed_storage)?;

        let cf = self.cf(<BlockChangeSet as Table>::NAME)?;
        let start = encode_block_number(expected_earliest_block.saturating_add(1));
        if let Some(end_block) = target_block.checked_add(1) {
            batch.delete_range_cf(&cf, start, encode_block_number(end_block));
        } else {
            batch.delete_range_cf(&cf, start, encode_block_number(u64::MAX));
            batch.delete_cf(&cf, encode_block_number(u64::MAX));
        }

        self.put_proof_window(
            &mut batch,
            ProofWindowKey::EarliestBlock,
            target_block,
            new_earliest_block_ref.block.hash,
        )?;

        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(counts)
    }

    fn get_initial_state_anchor(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        Ok(self.get_table::<ProofWindow>(ProofWindowKey::InitialStateAnchor)?.map(Into::into))
    }

    fn get_latest_history_key<T>(&self) -> BaseProofsStorageResult<Option<T::Key>>
    where
        T: RocksDbHistoryTable,
    {
        let cf = self.cf(T::NAME)?;
        let mut iter = self.db.iterator_cf(&cf, IteratorMode::End);
        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (raw_key, _) = item.map_err(rocksdb_error)?;
        decode_history_key::<T>(&raw_key).map(|(key, _)| Some(key)).map_err(Into::into)
    }
}

impl BaseProofsStore for RocksdbProofsStorage {
    type StorageTrieCursor<'tx>
        = RocksdbTrieCursor<'tx, StorageTrieHistory>
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = RocksdbTrieCursor<'tx, AccountTrieHistory>
    where
        Self: 'tx;
    type StorageCursor<'tx>
        = RocksdbStorageCursor<'tx>
    where
        Self: 'tx;
    type AccountHashedCursor<'tx>
        = RocksdbAccountCursor<'tx>
    where
        Self: 'tx;
    type Tx<'tx>
        = Arc<RocksdbReadSnapshot<'tx>>
    where
        Self: 'tx;

    fn ro_tx<'tx>(&'tx self) -> BaseProofsStorageResult<Self::Tx<'tx>> {
        Ok(Arc::new(RocksdbReadSnapshot::new(self.db.as_ref())))
    }

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_block_number_hash(ProofWindowKey::EarliestBlock)
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_latest_block_number_hash()
    }

    fn storage_trie_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        // Standalone cursor factories intentionally create independent snapshots. Use `ro_tx` and
        // the `*_with_tx` factories when multiple cursors need one consistent view.
        Ok(RocksdbTrieCursor::<StorageTrieHistory>::new(
            self.db.as_ref(),
            max_block_number,
            Some(hashed_address),
        ))
    }

    fn account_trie_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        Ok(RocksdbTrieCursor::<AccountTrieHistory>::new(self.db.as_ref(), max_block_number, None))
    }

    fn storage_hashed_cursor<'tx>(
        &'tx self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        Ok(RocksdbStorageCursor::new(self.db.as_ref(), max_block_number, hashed_address))
    }

    fn account_hashed_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        Ok(RocksdbAccountCursor::new(self.db.as_ref(), max_block_number))
    }

    fn storage_trie_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        Ok(RocksdbTrieCursor::<StorageTrieHistory>::new_with_snapshot(
            Arc::clone(tx),
            max_block_number,
            Some(hashed_address),
        ))
    }

    fn account_trie_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        Ok(RocksdbTrieCursor::<AccountTrieHistory>::new_with_snapshot(
            Arc::clone(tx),
            max_block_number,
            None,
        ))
    }

    fn storage_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_address: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        Ok(RocksdbStorageCursor::new_with_snapshot(
            Arc::clone(tx),
            max_block_number,
            hashed_address,
        ))
    }

    fn account_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        Ok(RocksdbAccountCursor::new_with_snapshot(Arc::clone(tx), max_block_number))
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        let counts =
            self.store_trie_updates_append_only(&mut batch, block_ref, block_state_diff)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(counts)
    }

    fn fetch_trie_updates(&self, block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        let snapshot = self.db.snapshot();
        let change_set = self
            .get_table_from_snapshot::<BlockChangeSet>(&snapshot, block_number)?
            .ok_or(BaseProofsStorageError::NoChangeSetForBlock(block_number))?;

        let mut trie_updates = TrieUpdates::default();
        for key in change_set.account_trie_keys {
            let entry = match get_history_exact::<AccountTrieHistory, _>(
                &snapshot,
                &self.db,
                key.clone(),
                block_number,
            )? {
                Some(value) if value.block_number == block_number => value.value.0,
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
            let entry = match get_history_exact::<StorageTrieHistory, _>(
                &snapshot,
                &self.db,
                key.clone(),
                block_number,
            )? {
                Some(value) if value.block_number == block_number => value.value.0,
                _ => {
                    return Err(BaseProofsStorageError::MissingStorageTrieHistory(
                        key.hashed_address,
                        key.path.0,
                        block_number,
                    ));
                }
            };

            let storage_updates = trie_updates
                .storage_tries
                .entry(key.hashed_address)
                .or_insert_with(StorageTrieUpdates::default);
            if let Some(value) = entry {
                storage_updates.storage_nodes.insert(key.path.0, value);
            } else {
                storage_updates.removed_nodes.insert(key.path.0);
            }
        }

        let mut post_state = HashedPostState::with_capacity(change_set.hashed_account_keys.len());
        for key in change_set.hashed_account_keys {
            let entry = match get_history_exact::<HashedAccountHistory, _>(
                &snapshot,
                &self.db,
                key,
                block_number,
            )? {
                Some(value) if value.block_number == block_number => value.value.0,
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
            let entry = match get_history_exact::<HashedStorageHistory, _>(
                &snapshot,
                &self.db,
                key.clone(),
                block_number,
            )? {
                Some(value) if value.block_number == block_number => value.value.0,
                _ => {
                    return Err(BaseProofsStorageError::MissingHashedStorageHistory {
                        hashed_address: key.hashed_address,
                        hashed_storage_key: key.hashed_storage_key,
                        block_number,
                    });
                }
            };

            let storage = post_state.storages.entry(key.hashed_address).or_default();
            if let Some(value) = entry {
                storage.storage.insert(key.hashed_storage_key, value.0);
            } else {
                storage.storage.insert(key.hashed_storage_key, U256::ZERO);
            }
        }

        Ok(BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        })
    }

    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let target_block = new_earliest_block_ref.block.number;
        let Some(prepared) = self.prepare_prune(target_block)? else {
            return Ok(WriteCounts::default());
        };

        self.commit_prepared_prune(prepared, new_earliest_block_ref)
    }

    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        let _guard = self.write_lock.lock();
        let Some(proof_window) = self.get_proof_window()? else {
            return Ok(());
        };

        if to.block.number > proof_window.latest.number {
            return Ok(());
        }

        if to.block.number <= proof_window.earliest.number {
            return Err(BaseProofsStorageError::UnwindBeyondEarliest {
                unwind_block_number: to.block.number,
                earliest_block_number: proof_window.earliest.number,
            });
        }

        // Keep collection and deletion under the same write lock so another RocksDB writer cannot
        // change the proof window or history rows between choosing keys and committing the batch.
        let history_to_delete = self.collect_history_ranged(to.block.number..)?;
        let mut batch = WriteBatch::default();
        self.delete_history_ranged(&mut batch, history_to_delete)?;
        self.put_proof_window(
            &mut batch,
            ProofWindowKey::LatestBlock,
            to.block.number.saturating_sub(1),
            to.parent,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn replace_updates(
        &self,
        latest_common_block: BlockNumHash,
        mut blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        blocks_to_add.sort_unstable_by_key(|(block, _)| block.block.number);

        let mut latest_block_hash = latest_common_block.hash;
        for (block_with_parent, _) in &blocks_to_add {
            let block_number = block_with_parent.block.number;
            if latest_block_hash != block_with_parent.parent {
                return Err(BaseProofsStorageError::OutOfOrder {
                    block_number,
                    parent_block_hash: block_with_parent.parent,
                    latest_block_hash,
                });
            }
            latest_block_hash = block_with_parent.block.hash;
        }

        let _guard = self.write_lock.lock();
        let history_to_delete = if let Some(start_block) = latest_common_block.number.checked_add(1)
        {
            self.collect_history_ranged(start_block..)?
        } else {
            RocksdbHistoryDeleteBatch::default()
        };
        let mut batch = WriteBatch::default();
        self.delete_history_ranged(&mut batch, history_to_delete)?;
        self.put_proof_window(
            &mut batch,
            ProofWindowKey::LatestBlock,
            latest_common_block.number,
            latest_common_block.hash,
        )?;

        let mut replacement_state = RocksdbReplacementState::default();
        for (block_with_parent, diff) in blocks_to_add {
            self.store_replacement_trie_updates_append_only(
                &mut batch,
                latest_common_block.number,
                &mut replacement_state,
                block_with_parent,
                diff,
            )?;
        }

        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn set_earliest_block_number(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.set_earliest_block_number_hash(block_number, hash)
    }
}

impl BaseProofsInitialStateStore for RocksdbProofsStorage {
    fn initial_state_anchor(&self) -> BaseProofsStorageResult<InitialStateAnchor> {
        let Some(block) = self.get_initial_state_anchor()? else {
            return Ok(InitialStateAnchor::default());
        };

        let completed = self.get_earliest_block_number()?.is_some();

        Ok(InitialStateAnchor {
            block: Some(block),
            status: if completed {
                InitialStateStatus::Completed
            } else {
                InitialStateStatus::InProgress
            },
            latest_account_trie_key: self.get_latest_history_key::<AccountTrieHistory>()?,
            latest_storage_trie_key: self.get_latest_history_key::<StorageTrieHistory>()?,
            latest_hashed_account_key: self.get_latest_history_key::<HashedAccountHistory>()?,
            latest_hashed_storage_key: self.get_latest_history_key::<HashedStorageHistory>()?,
        })
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        let _guard = self.write_lock.lock();
        if self.get_initial_state_anchor()?.is_some() {
            return Err(DatabaseError::Other("initial state anchor already set".to_owned()).into());
        }

        let mut batch = WriteBatch::default();
        self.put_table::<ProofWindow>(
            &mut batch,
            ProofWindowKey::InitialStateAnchor,
            &anchor.into(),
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
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
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<AccountTrieHistory, _, _>(
            &mut batch,
            0,
            account_nodes.into_iter(),
            true,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
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
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<StorageTrieHistory, _, _>(
            &mut batch,
            0,
            storage_nodes.into_iter().map(|(path, node)| (hashed_address, path, node)),
            true,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn store_hashed_accounts(
        &self,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> BaseProofsStorageResult<()> {
        let mut accounts = accounts;
        if accounts.is_empty() {
            return Ok(());
        }

        accounts.sort_by_key(|(key, _)| *key);
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<HashedAccountHistory, _, _>(
            &mut batch,
            0,
            accounts.into_iter(),
            true,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
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

        storages.sort_by_key(|(key, _)| *key);
        let _guard = self.write_lock.lock();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<HashedStorageHistory, _, _>(
            &mut batch,
            0,
            storages
                .into_iter()
                .map(|(key, value)| (hashed_address, key, Some(StorageValue(value)))),
            true,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        self.set_earliest_block_number(anchor.number, anchor.hash)?;
        Ok(anchor)
    }
}

#[cfg(feature = "metrics")]
impl reth_db::database_metrics::DatabaseMetrics for RocksdbProofsStorage {
    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        let mut metrics = Vec::new();

        for table in Self::column_families() {
            let Some(cf) = self.db.cf_handle(table) else {
                continue;
            };

            let estimated_num_keys = self
                .db
                .property_int_value_cf(&cf, rocksdb::properties::ESTIMATE_NUM_KEYS)
                .ok()
                .flatten()
                .unwrap_or(0);
            let sst_size = self
                .db
                .property_int_value_cf(&cf, rocksdb::properties::LIVE_SST_FILES_SIZE)
                .ok()
                .flatten()
                .unwrap_or(0);
            let memtable_size = self
                .db
                .property_int_value_cf(&cf, rocksdb::properties::SIZE_ALL_MEM_TABLES)
                .ok()
                .flatten()
                .unwrap_or(0);
            let pending_compaction_bytes = self
                .db
                .property_int_value_cf(&cf, rocksdb::properties::ESTIMATE_PENDING_COMPACTION_BYTES)
                .ok()
                .flatten()
                .unwrap_or(0);

            metrics.push((
                "base_proof_storage.table_size",
                (sst_size + memtable_size) as f64,
                vec![Label::new("table", table)],
            ));
            metrics.push((
                "base_proof_storage.table_entries",
                estimated_num_keys as f64,
                vec![Label::new("table", table)],
            ));
            metrics.push((
                "base_proof_storage.pending_compaction_bytes",
                pending_compaction_bytes as f64,
                vec![Label::new("table", table)],
            ));
            metrics.push((
                "base_proof_storage.sst_size",
                sst_size as f64,
                vec![Label::new("table", table)],
            ));
            metrics.push((
                "base_proof_storage.memtable_size",
                memtable_size as f64,
                vec![Label::new("table", table)],
            ));
        }

        let wal_size: u64 = std::fs::read_dir(self.db.path())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
                    .filter_map(|entry| entry.metadata().ok())
                    .map(|metadata| metadata.len())
                    .sum()
            })
            .unwrap_or(0);

        metrics.push(("base_proof_storage.wal_size", wal_size as f64, vec![]));

        metrics
    }
}

#[cfg(not(feature = "metrics"))]
impl reth_db::database_metrics::DatabaseMetrics for RocksdbProofsStorage {}

impl<'db, T, V> RocksdbVersionedCursor<'db, T>
where
    T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
    T: RocksDbHistoryTable,
    T::Key: Default,
    T::Value: Decompress,
{
    /// Creates a cursor over a `RocksDB` history column family.
    fn new(db: &'db RocksDb, max_block_number: u64) -> Self {
        let snapshot = Arc::new(RocksdbReadSnapshot::new(db));
        Self::new_with_snapshot(snapshot, max_block_number)
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
    ) -> Self {
        Self { snapshot, max_block_number, current_key: None, _table: PhantomData }
    }

    fn cf(&self) -> Result<Arc<BoundColumnFamily<'_>>, DatabaseError> {
        self.snapshot.cf(T::NAME)
    }

    fn latest_version_for_key(&self, key: T::Key) -> RocksDbLatestVersionResult<T> {
        let cf = self.cf()?;
        let prefix = encode_history_key_prefix::<T>(&key);
        let target = encode_history_key::<T>(&key, self.max_block_number);
        let mut iter = self
            .snapshot
            .snapshot
            .iterator_cf(&cf, IteratorMode::From(&target, Direction::Reverse));

        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
        if !raw_key.starts_with(&prefix) {
            return Ok(None);
        }

        let (decoded_key, _) = decode_history_key::<T>(&raw_key)?;
        let value = T::Value::decompress(&raw_value)?;
        Ok(Some((decoded_key, value)))
    }

    fn seek_exact(&mut self, key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        self.current_key = Some(key.clone());
        if let Some((latest_key, latest_value)) = self.latest_version_for_key(key)?
            && let MaybeDeleted(Some(value)) = latest_value.value
        {
            return Ok(Some((latest_key, value)));
        }
        Ok(None)
    }

    fn seek(&mut self, start_key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        let Some(first_key) = self.first_key_ge(start_key)? else {
            self.current_key = None;
            return Ok(None);
        };
        self.next_live_from(first_key)
    }

    fn next(&mut self) -> Result<Option<(T::Key, V)>, DatabaseError> {
        let next_key = if let Some(key) = self.current_key.clone() {
            self.first_key_gt(key)?
        } else {
            self.first_key_ge(T::Key::default())?
        };

        let Some(next_key) = next_key else {
            self.current_key = None;
            return Ok(None);
        };
        self.next_live_from(next_key)
    }

    fn next_live_from(&mut self, mut key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        loop {
            self.current_key = Some(key.clone());
            if let Some((live_key, value)) = self.seek_exact(key.clone())? {
                return Ok(Some((live_key, value)));
            }

            let Some(next_key) = self.first_key_gt(key)? else {
                self.current_key = None;
                return Ok(None);
            };
            key = next_key;
        }
    }

    fn first_key_ge(&self, key: T::Key) -> Result<Option<T::Key>, DatabaseError> {
        let cf = self.cf()?;
        let start_key = encode_history_key::<T>(&key, 0);
        let mut iter = self
            .snapshot
            .snapshot
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));
        decode_next_history_key::<T>(&mut iter)
    }

    fn first_key_gt(&self, key: T::Key) -> Result<Option<T::Key>, DatabaseError> {
        let cf = self.cf()?;
        let start_key = encode_history_key::<T>(&key, u64::MAX);
        let iter = self
            .snapshot
            .snapshot
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));

        for item in iter {
            let (raw_key, _) = item.map_err(rocksdb_error)?;
            let (next_key, _) = decode_history_key::<T>(&raw_key)?;
            if next_key > key {
                return Ok(Some(next_key));
            }
        }

        Ok(None)
    }

    const fn is_positioned(&self) -> bool {
        self.current_key.is_some()
    }
}

impl<'db> RocksdbTrieCursor<'db, AccountTrieHistory> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: Option<B256>) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: Option<B256>,
    ) -> Self {
        Self {
            inner: RocksdbVersionedCursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }
}

impl<'db> RocksdbTrieCursor<'db, StorageTrieHistory> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: Option<B256>) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: Option<B256>,
    ) -> Self {
        Self {
            inner: RocksdbVersionedCursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }
}

impl TrieCursor for RocksdbTrieCursor<'_, AccountTrieHistory> {
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self
            .inner
            .seek_exact(StoredNibbles(path))?
            .map(|(StoredNibbles(nibbles), node)| (nibbles, node)))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self
            .inner
            .seek(StoredNibbles(path))?
            .map(|(StoredNibbles(nibbles), node)| (nibbles, node)))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        Ok(self.inner.next()?.map(|(StoredNibbles(nibbles), node)| (nibbles, node)))
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.inner.current_key.clone().map(|StoredNibbles(nibbles)| nibbles))
    }

    fn reset(&mut self) {
        self.inner.current_key = None;
    }
}

impl TrieCursor for RocksdbTrieCursor<'_, StorageTrieHistory> {
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        let key = StorageTrieKey::new(address, StoredNibbles(path));
        Ok(self
            .inner
            .seek_exact(key)?
            .and_then(|(key, node)| (key.hashed_address == address).then_some((key.path.0, node))))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        let key = StorageTrieKey::new(address, StoredNibbles(path));
        Ok(self
            .inner
            .seek(key)?
            .and_then(|(key, node)| (key.hashed_address == address).then_some((key.path.0, node))))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        if !self.inner.is_positioned() {
            return self.seek(Nibbles::default());
        }
        Ok(self
            .inner
            .next()?
            .and_then(|(key, node)| (key.hashed_address == address).then_some((key.path.0, node))))
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        Ok(self
            .inner
            .current_key
            .clone()
            .and_then(|key| (key.hashed_address == address).then_some(key.path.0)))
    }

    fn reset(&mut self) {
        self.inner.current_key = None;
    }
}

impl TrieStorageCursor for RocksdbTrieCursor<'_, StorageTrieHistory> {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = Some(hashed_address);
        self.inner.current_key = None;
    }
}

impl<'db> RocksdbStorageCursor<'db> {
    /// Creates a `RocksDB` storage cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: B256) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: B256,
    ) -> Self {
        Self {
            inner: RocksdbVersionedCursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }
}

impl HashedCursor for RocksdbStorageCursor<'_> {
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let storage_key = HashedStorageKey::new(self.hashed_address, key);
        let result = self.inner.seek(storage_key)?.and_then(|(key, value)| {
            (key.hashed_address == self.hashed_address).then_some((key.hashed_storage_key, value.0))
        });

        if let Some((_, value)) = result
            && value.is_zero()
        {
            return self.next();
        }

        Ok(result)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.inner.is_positioned() {
            return self.seek(B256::ZERO);
        }

        loop {
            let result = self.inner.next()?.and_then(|(key, value)| {
                (key.hashed_address == self.hashed_address)
                    .then_some((key.hashed_storage_key, value.0))
            });

            let Some((key, value)) = result else {
                return Ok(None);
            };
            if value.is_zero() {
                continue;
            }
            return Ok(Some((key, value)));
        }
    }

    fn reset(&mut self) {
        self.inner.current_key = None;
    }
}

impl HashedStorageCursor for RocksdbStorageCursor<'_> {
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.inner.current_key = None;
    }
}

impl<'db> RocksdbAccountCursor<'db> {
    /// Creates a `RocksDB` account cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number) }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
    ) -> Self {
        Self { inner: RocksdbVersionedCursor::new_with_snapshot(snapshot, max_block_number) }
    }
}

impl HashedCursor for RocksdbAccountCursor<'_> {
    type Value = Account;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.inner.seek(key)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.inner.next()
    }

    fn reset(&mut self) {
        self.inner.current_key = None;
    }
}

fn rocksdb_error(error: rocksdb::Error) -> DatabaseError {
    DatabaseError::Other(error.to_string())
}

fn encode_table_key<T: Table>(key: T::Key) -> Vec<u8> {
    key.encode().as_ref().to_vec()
}

fn encode_table_value<T: Table>(value: &T::Value) -> Vec<u8> {
    let mut encoded = <T::Value as Compress>::Compressed::default();
    value.compress_to_buf(&mut encoded);
    encoded.into()
}

fn encode_history_key<T>(key: &T::Key, block_number: u64) -> Vec<u8>
where
    T: RocksDbHistoryTable,
{
    let mut encoded = encode_history_key_prefix::<T>(key);
    encoded.extend_from_slice(&block_number.to_be_bytes());
    encoded
}

fn encode_history_key_prefix<T>(key: &T::Key) -> Vec<u8>
where
    T: RocksDbHistoryTable,
{
    T::encode_history_key_prefix(key)
}

fn decode_history_key<T>(raw_key: &[u8]) -> Result<(T::Key, u64), DatabaseError>
where
    T: RocksDbHistoryTable,
{
    if raw_key.len() != T::KEY_LEN + BLOCK_NUMBER_KEY_LEN {
        return Err(DatabaseError::Decode);
    }
    let split = T::KEY_LEN;
    let key = T::decode_history_key_prefix(&raw_key[..split])?;
    let block_number =
        u64::from_be_bytes(raw_key[split..].try_into().map_err(|_| DatabaseError::Decode)?);
    Ok((key, block_number))
}

fn decode_next_history_key<T>(
    iter: &mut DBIteratorWithThreadMode<'_, RocksDb>,
) -> Result<Option<T::Key>, DatabaseError>
where
    T: RocksDbHistoryTable,
{
    let Some(item) = iter.next() else {
        return Ok(None);
    };
    let (raw_key, _) = item.map_err(rocksdb_error)?;
    decode_history_key::<T>(&raw_key).map(|(key, _)| Some(key))
}

fn get_history_exact<T, V>(
    snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
    db: &Arc<RocksDb>,
    key: T::Key,
    block_number: u64,
) -> BaseProofsStorageResult<Option<T::Value>>
where
    T: Table<Value = VersionedValue<V>> + DupSort<SubKey = u64>,
    T::Key: Clone,
    T::Value: Decompress,
    T: RocksDbHistoryTable,
{
    let cf = db.cf_handle(T::NAME).ok_or_else(|| {
        DatabaseError::Other(format!("missing RocksDB column family {}", T::NAME))
    })?;
    snapshot
        .get_cf(&cf, encode_history_key::<T>(&key, block_number))
        .map_err(rocksdb_error)?
        .map(|value| T::Value::decompress(&value).map_err(Into::into))
        .transpose()
}

impl RocksDbHistoryTable for AccountTrieHistory {
    const KEY_LEN: usize = PACKED_NIBBLES_KEY_LEN;

    fn encode_history_key_prefix(key: &Self::Key) -> Vec<u8> {
        encode_packed_nibbles(&key.0).to_vec()
    }

    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        decode_packed_nibbles(raw_key).map(StoredNibbles)
    }
}

impl RocksDbHistoryTable for StorageTrieHistory {
    const KEY_LEN: usize = HASH_KEY_LEN + PACKED_NIBBLES_KEY_LEN;

    fn encode_history_key_prefix(key: &Self::Key) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(&encode_packed_nibbles(&key.path.0));
        encoded
    }

    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        let hashed_address = B256::from_slice(&raw_key[..HASH_KEY_LEN]);
        let path = StoredNibbles(decode_packed_nibbles(&raw_key[HASH_KEY_LEN..])?);
        Ok(StorageTrieKey::new(hashed_address, path))
    }
}

impl RocksDbHistoryTable for HashedAccountHistory {
    const KEY_LEN: usize = HASH_KEY_LEN;

    fn encode_history_key_prefix(key: &Self::Key) -> Vec<u8> {
        key.as_slice().to_vec()
    }

    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        Ok(B256::from_slice(raw_key))
    }
}

impl RocksDbHistoryTable for HashedStorageHistory {
    const KEY_LEN: usize = HASH_KEY_LEN * 2;

    fn encode_history_key_prefix(key: &Self::Key) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(key.hashed_storage_key.as_slice());
        encoded
    }

    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        Ok(HashedStorageKey::new(
            B256::from_slice(&raw_key[..HASH_KEY_LEN]),
            B256::from_slice(&raw_key[HASH_KEY_LEN..]),
        ))
    }
}

fn encode_packed_nibbles(nibbles: &Nibbles) -> [u8; PACKED_NIBBLES_KEY_LEN] {
    assert!(nibbles.len() <= 64, "trie paths must fit within 64 nibbles");

    let mut encoded = [0; PACKED_NIBBLES_KEY_LEN];
    nibbles.pack_to(&mut encoded[..HASH_KEY_LEN]);
    encoded[HASH_KEY_LEN] = nibbles.len() as u8;
    encoded
}

fn decode_packed_nibbles(raw_key: &[u8]) -> Result<Nibbles, DatabaseError> {
    if raw_key.len() != PACKED_NIBBLES_KEY_LEN {
        return Err(DatabaseError::Decode);
    }

    let nibble_count = raw_key[HASH_KEY_LEN] as usize;
    if nibble_count > 64 {
        return Err(DatabaseError::Decode);
    }

    let packed_len = nibble_count.div_ceil(2);
    if nibble_count % 2 == 1 && raw_key[packed_len - 1] & 0x0f != 0 {
        return Err(DatabaseError::Decode);
    }
    if raw_key[packed_len..HASH_KEY_LEN].iter().any(|byte| *byte != 0) {
        return Err(DatabaseError::Decode);
    }

    let mut nibbles = Vec::with_capacity(nibble_count);
    for index in 0..nibble_count {
        let byte = raw_key[index / 2];
        let nibble = if index % 2 == 0 { byte >> 4 } else { byte & 0x0f };
        nibbles.push(nibble);
    }
    Ok(Nibbles::from_nibbles_unchecked(nibbles))
}

const fn encode_block_number(block_number: u64) -> [u8; 8] {
    block_number.to_be_bytes()
}

fn decode_block_number(raw_key: &[u8]) -> Result<u64, DatabaseError> {
    if raw_key.len() != 8 {
        return Err(DatabaseError::Decode);
    }
    Ok(u64::from_be_bytes(raw_key.try_into().map_err(|_| DatabaseError::Decode)?))
}

fn range_start(range: &impl RangeBounds<u64>) -> u64 {
    match range.start_bound() {
        Bound::Included(start) => *start,
        Bound::Excluded(start) => start.saturating_add(1),
        Bound::Unbounded => 0,
    }
}

fn flatten_and_sort<K: Ord>(map: HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut values: Vec<_> = map.into_iter().collect();
    values.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_nibbles_round_trip() {
        let nibbles = Nibbles::from_nibbles_unchecked([0, 1, 0, 2, 15, 0, 3]);
        let encoded = encode_packed_nibbles(&nibbles);

        assert_eq!(encoded[HASH_KEY_LEN], 7);
        assert_eq!(decode_packed_nibbles(&encoded).unwrap(), nibbles);
    }

    #[test]
    fn packed_nibbles_preserve_lexicographic_order() {
        let keys = [
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 1],
            vec![1],
            vec![1, 0],
            vec![1, 1],
            vec![2],
            vec![15],
            vec![15, 15],
        ];

        for left in &keys {
            for right in &keys {
                let left = Nibbles::from_nibbles_unchecked(left);
                let right = Nibbles::from_nibbles_unchecked(right);
                assert_eq!(
                    left.cmp(&right),
                    encode_packed_nibbles(&left).cmp(&encode_packed_nibbles(&right))
                );
            }
        }
    }

    #[test]
    fn hashed_history_keys_preserve_full_byte_ordering() {
        let keys = [B256::ZERO, B256::repeat_byte(2), B256::repeat_byte(255)];

        for left in keys {
            for right in keys {
                assert_eq!(
                    left.cmp(&right),
                    encode_history_key_prefix::<HashedAccountHistory>(&left)
                        .cmp(&encode_history_key_prefix::<HashedAccountHistory>(&right))
                );
            }
        }
    }

    #[test]
    fn packed_nibbles_reject_invalid_padding() {
        let mut encoded = encode_packed_nibbles(&Nibbles::from_nibbles_unchecked([1, 2, 3]));
        encoded[HASH_KEY_LEN - 1] = 1;
        assert!(decode_packed_nibbles(&encoded).is_err());

        let mut encoded = encode_packed_nibbles(&Nibbles::from_nibbles_unchecked([1]));
        encoded[0] |= 1;
        assert!(decode_packed_nibbles(&encoded).is_err());
    }
}
