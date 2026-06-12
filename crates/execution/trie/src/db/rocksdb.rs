//! `RocksDB` implementation of proofs storage.

use std::{
    collections::BTreeMap,
    fmt,
    marker::PhantomData,
    ops::{Bound, RangeBounds},
    path::Path,
    sync::Arc,
    time::Instant,
};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256, map::HashMap};
#[cfg(feature = "metrics")]
use metrics::Label;
use parking_lot::{Mutex, RwLock};
use reth_db::{
    DatabaseError,
    table::{Compress, Decompress, DupSort, Encode, Table},
};
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles, StoredNibbles};
use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, BoundColumnFamily, Cache, ColumnFamilyDescriptor,
    CompactionPri, DBCompressionType, DBWithThreadMode, Direction, IteratorMode, MultiThreaded,
    Options, ReadOptions, SliceTransform, SnapshotWithThreadMode, WriteBatch, WriteOptions,
};
use tracing::info;

use super::{BlockNumberHash, ProofWindow, ProofWindowKey};
use crate::{
    BaseProofsStorageError,
    BaseProofsStorageError::NoBlocksFound,
    BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    api::{
        BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsInitialStateStore,
        InitialStateAnchor, InitialStateStatus, WriteCounts,
    },
    db::{
        AccountTrieHistory, BlockChangeSet, ChangeSet, HashedAccountHistory, HashedStorageHistory,
        HashedStorageKey, IntoKV, MaybeDeleted, StorageTrieHistory, StorageTrieKey, StorageValue,
        VersionedValue,
    },
};

type RocksDb = DBWithThreadMode<MultiThreaded>;
/// Result type for looking up the latest visible version of a history-table key.
pub type RocksDbLatestVersionResult<T> =
    Result<Option<(<T as Table>::Key, <T as Table>::Value)>, DatabaseError>;

const HASH_KEY_LEN: usize = 32;
const PACKED_NIBBLES_KEY_LEN: usize = 33;
const BLOCK_NUMBER_KEY_LEN: usize = 8;
const DEFAULT_BLOCK_CACHE_SIZE: usize = 1024 << 20;
const DEFAULT_BLOCK_SIZE: usize = 16 * 1024;
const DEFAULT_BYTES_PER_SYNC: u64 = 4_194_304;
const DEFAULT_COMPACTION_READAHEAD_SIZE: usize = 0;
const DEFAULT_DIRECT_IO_FOR_FLUSH_AND_COMPACTION: bool = true;
const DEFAULT_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER: i32 = 4;
const DEFAULT_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER: i32 = 20;
const DEFAULT_LEVEL_ZERO_STOP_WRITES_TRIGGER: i32 = 36;
const DEFAULT_MAX_BACKGROUND_JOBS: i32 = 8;
const DEFAULT_MAX_SUBCOMPACTIONS: u32 = 1;
const DEFAULT_MAX_OPEN_FILES: i32 = -1;
const DEFAULT_MAX_WRITE_BUFFER_NUMBER: i32 = 3;
const DEFAULT_TARGET_FILE_SIZE_BASE: u64 = 256 * 1024 * 1024;
const DEFAULT_WRITE_BUFFER_SIZE: usize = 64 * 1024 * 1024;
const DEFAULT_BLOOM_BITS_PER_KEY: f64 = 10.0;

/// Options for opening [`RocksdbProofsStorage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RocksdbProofsStorageOptions {
    /// LRU block cache size in bytes.
    pub block_cache_size: usize,
    /// Number of bytes `RocksDB` should write before asking the OS to start syncing.
    pub bytes_per_sync: u64,
    /// Readahead size in bytes for compaction input reads.
    pub compaction_readahead_size: usize,
    /// Number of L0 files that triggers compaction.
    pub level_zero_file_num_compaction_trigger: i32,
    /// Number of L0 files that triggers write slowdown.
    pub level_zero_slowdown_writes_trigger: i32,
    /// Number of L0 files that stops writes.
    pub level_zero_stop_writes_trigger: i32,
    /// Maximum number of `RocksDB` background jobs.
    pub max_background_jobs: i32,
    /// Maximum number of subcompactions per compaction.
    pub max_subcompactions: u32,
    /// Maximum total WAL size in bytes.
    pub max_total_wal_size: Option<u64>,
    /// Maximum number of write buffers per column family.
    pub max_write_buffer_number: i32,
    /// Write buffer size per column family in bytes.
    pub write_buffer_size: usize,
    /// Base target file size in bytes.
    pub target_file_size_base: u64,
    /// Whether flush and compaction files should use direct I/O.
    pub use_direct_io_for_flush_and_compaction: bool,
}

impl Default for RocksdbProofsStorageOptions {
    fn default() -> Self {
        Self {
            block_cache_size: DEFAULT_BLOCK_CACHE_SIZE,
            bytes_per_sync: DEFAULT_BYTES_PER_SYNC,
            compaction_readahead_size: DEFAULT_COMPACTION_READAHEAD_SIZE,
            level_zero_file_num_compaction_trigger: DEFAULT_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER,
            level_zero_slowdown_writes_trigger: DEFAULT_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER,
            level_zero_stop_writes_trigger: DEFAULT_LEVEL_ZERO_STOP_WRITES_TRIGGER,
            max_background_jobs: DEFAULT_MAX_BACKGROUND_JOBS,
            max_subcompactions: DEFAULT_MAX_SUBCOMPACTIONS,
            max_total_wal_size: None,
            max_write_buffer_number: DEFAULT_MAX_WRITE_BUFFER_NUMBER,
            write_buffer_size: DEFAULT_WRITE_BUFFER_SIZE,
            target_file_size_base: DEFAULT_TARGET_FILE_SIZE_BASE,
            use_direct_io_for_flush_and_compaction: DEFAULT_DIRECT_IO_FOR_FLUSH_AND_COMPACTION,
        }
    }
}

impl RocksdbProofsStorageOptions {
    fn max_total_wal_size(self, column_family_count: usize) -> u64 {
        self.max_total_wal_size.unwrap_or_else(|| {
            column_family_count as u64
                * self.write_buffer_size as u64
                * self.max_write_buffer_number.max(1) as u64
        })
    }
}

/// Sealed marker trait for `RocksDB` history column families.
///
/// Implemented by the four history tables: [`AccountTrieHistory`], [`StorageTrieHistory`],
/// [`HashedAccountHistory`], and [`HashedStorageHistory`].  The blanket impl provides the
/// key-encoding and decoding logic needed by the versioned cursor.
pub trait RocksDbHistoryTable: Table + DupSort<SubKey = u64> {
    /// Fixed encoded table-key length before the block-number suffix.
    const KEY_LEN: usize;

    /// Encodes the table key prefix used before the block-number suffix.
    fn encode_history_key_prefix(key: &Self::Key) -> Result<Vec<u8>, DatabaseError>;

    /// Decodes the table key prefix used before the block-number suffix.
    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError>;
}

/// `RocksDB` implementation of [`BaseProofsStore`].
pub struct RocksdbProofsStorage {
    db: Arc<RocksDb>,
    write_options: WriteOptions,
    // Serializes append-only writers that read LatestBlock before writing LatestBlock + 1.
    /// Lock guarding append-only writes that advance `LatestBlock`.
    append_lock: Mutex<()>,
    // Serializes prune plans that assume a stable EarliestBlock across prepare and commit.
    /// Lock guarding prune prepare/commit sequences over `EarliestBlock`.
    prune_lock: Mutex<()>,
    // Append and prune share read access. History rewrites take write access.
    history_gate: RwLock<()>,
}

/// Preprocessed prune plan for a target block number.
#[derive(Debug, Clone)]
pub struct RocksdbPrunePlan {
    /// Earliest block number currently retained before pruning.
    pub earliest_block: u64,
    /// Hash of the earliest retained block.
    pub earliest_hash: B256,
    /// Account trie keys and survivor block numbers that must be kept.
    pub acc_survivors: Vec<(StoredNibbles, u64)>,
    storage_survivors: Vec<(StorageTrieKey, u64)>,
    hashed_acc_survivors: Vec<(B256, u64)>,
    hashed_storage_survivors: Vec<(HashedStorageKey, u64)>,
}

impl RocksdbPrunePlan {
    const fn total_survivors(&self) -> usize {
        self.acc_survivors.len()
            + self.storage_survivors.len()
            + self.hashed_acc_survivors.len()
            + self.hashed_storage_survivors.len()
    }
}

/// Preprocessed delete work for a prune commit.
#[derive(Debug, Clone)]
pub struct RocksdbPreparedPrune {
    /// Earliest block number expected when applying this prepared prune.
    pub expected_earliest_block: u64,
    /// Earliest block hash expected when applying this prepared prune.
    pub expected_earliest_hash: B256,
    /// New earliest block number after prune commit.
    pub target_block: u64,
    /// Hash of the new earliest block after prune commit.
    pub target_hash: B256,
    /// Raw history keys grouped by table for deletion.
    pub deletes: RocksdbPreparedHistoryDeletes,
    /// Write counters for delete operations in this prune batch.
    pub counts: WriteCounts,
}

/// Raw history keys to delete during a prune commit.
#[derive(Debug, Default, Clone)]
pub struct RocksdbPreparedHistoryDeletes {
    /// Raw account trie history keys scheduled for deletion.
    pub account_trie: Vec<Vec<u8>>,
    /// Raw storage trie history keys scheduled for deletion.
    pub storage_trie: Vec<Vec<u8>>,
    /// Raw hashed account history keys scheduled for deletion.
    pub hashed_account: Vec<Vec<u8>>,
    /// Raw hashed storage history keys scheduled for deletion.
    pub hashed_storage: Vec<Vec<u8>>,
}

impl RocksdbPreparedHistoryDeletes {
    const fn total(&self) -> usize {
        self.account_trie.len()
            + self.storage_trie.len()
            + self.hashed_account.len()
            + self.hashed_storage.len()
    }
}

/// Preprocessed delete work for a prune range.
#[derive(Debug, Default)]
pub struct RocksdbHistoryDeleteBatch {
    /// Block numbers whose `BlockChangeSet` rows should be removed.
    pub block_numbers: Vec<u64>,
    /// Account trie history keys and block numbers to delete.
    pub account_trie: Vec<(<AccountTrieHistory as Table>::Key, u64)>,
    /// Storage trie history keys and block numbers to delete.
    pub storage_trie: Vec<(<StorageTrieHistory as Table>::Key, u64)>,
    /// Hashed account history keys and block numbers to delete.
    pub hashed_account: Vec<(<HashedAccountHistory as Table>::Key, u64)>,
    /// Hashed storage history keys and block numbers to delete.
    pub hashed_storage: Vec<(<HashedStorageHistory as Table>::Key, u64)>,
}

/// Request-scoped read snapshot for [`RocksdbProofsStorage`].
///
/// This type is public because it is the [`BaseProofsStore::Tx`] associated type for the
/// `RocksDB` backend. Callers that need several cursors to read the same database view should
/// acquire one snapshot with [`BaseProofsStore::ro_tx`] and pass it to the `*_with_tx` cursor
/// factories.
pub struct RocksdbReadSnapshot<'db> {
    /// Underlying `RocksDB` database handle.
    pub db: &'db RocksDb,
    /// Snapshot pinned for consistent reads.
    pub snapshot: SnapshotWithThreadMode<'db, RocksDb>,
}

/// Cursor over `RocksDB` versioned history rows.
pub struct RocksdbVersionedCursor<'db, T: Table + DupSort> {
    /// Shared read snapshot used for cursor lookups.
    pub snapshot: Arc<RocksdbReadSnapshot<'db>>,
    /// Maximum block number visible to this cursor.
    pub max_block_number: u64,
    /// Current logical key position in the cursor.
    pub current_key: Option<T::Key>,
    /// Marker for the table type parameter.
    pub _table: PhantomData<T>,
}

/// `RocksDB` implementation of [`TrieCursor`].
pub struct RocksdbTrieCursor<'db, T: Table + DupSort> {
    /// Underlying versioned history cursor.
    pub inner: RocksdbVersionedCursor<'db, T>,
    /// Optional hashed address scope for storage-trie iteration.
    pub hashed_address: Option<B256>,
}

/// `RocksDB` implementation of [`HashedCursor`] for storage state.
pub struct RocksdbStorageCursor<'db> {
    /// Underlying versioned history cursor.
    pub inner: RocksdbVersionedCursor<'db, HashedStorageHistory>,
    /// Address whose storage slots are being iterated.
    pub hashed_address: B256,
}

/// `RocksDB` implementation of [`HashedCursor`] for account state.
pub struct RocksdbAccountCursor<'db> {
    /// Underlying versioned history cursor.
    pub inner: RocksdbVersionedCursor<'db, HashedAccountHistory>,
}

/// In-memory replacement overlays for wiped storage trie and hashed storage state.
#[derive(Debug, Default)]
pub struct RocksdbReplacementState {
    /// Replacement entries for storage trie nodes keyed by address/path.
    pub storage_trie: BTreeMap<StorageTrieKey, Option<BranchNodeCompact>>,
    /// Replacement entries for hashed storage slots keyed by address/slot.
    pub hashed_storage: BTreeMap<HashedStorageKey, Option<StorageValue>>,
}

/// Earliest and latest block/hash pair currently retained in proof storage.
#[derive(Debug, Clone, Copy)]
pub struct ProofWindowValue {
    /// Earliest retained block/hash pair.
    pub earliest: NumHash,
    /// Latest retained block/hash pair.
    pub latest: NumHash,
}

impl fmt::Debug for RocksdbProofsStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbProofsStorage").finish_non_exhaustive()
    }
}

impl<'db> RocksdbReadSnapshot<'db> {
    /// Creates a new read snapshot wrapper for the given database.
    pub fn new(db: &'db RocksDb) -> Self {
        let snapshot = db.snapshot();
        Self { db, snapshot }
    }

    /// Returns a column family handle by name from this snapshot's database.
    pub fn cf(&self, name: &'static str) -> Result<Arc<BoundColumnFamily<'_>>, DatabaseError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| DatabaseError::Other(format!("missing RocksDB column family {name}")))
    }

    /// Returns the underlying `RocksDB` snapshot.
    pub const fn snapshot(&self) -> &SnapshotWithThreadMode<'db, RocksDb> {
        &self.snapshot
    }
}

static_assertions::assert_impl_all!(RocksdbReadSnapshot<'static>: Send, Sync);

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
        Self::new_with_options(path, RocksdbProofsStorageOptions::default())
    }

    /// Flushes and compacts every proofs-history column family.
    pub fn flush_and_compact(&self) -> BaseProofsStorageResult<()> {
        for name in Self::column_families() {
            let cf = self.cf(name)?;
            self.db.flush_cf(&cf).map_err(rocksdb_error)?;
            self.db.compact_range_cf::<&[u8], &[u8]>(&cf, None, None);
        }

        Ok(())
    }

    /// Creates a new [`RocksdbProofsStorage`] instance with the given path and options.
    pub fn new_with_options(
        path: &Path,
        storage_options: RocksdbProofsStorageOptions,
    ) -> Result<Self, BaseProofsStorageError> {
        let block_cache = Cache::new_lru_cache(storage_options.block_cache_size);
        let db_options = Self::db_options(&block_cache, storage_options);
        let descriptors = Self::column_families().into_iter().map(|name| {
            ColumnFamilyDescriptor::new(name, Self::cf_options(name, &block_cache, storage_options))
        });
        let db = RocksDb::open_cf_descriptors(&db_options, path, descriptors)
            .map_err(|e| DatabaseError::Other(format!("failed to open RocksDB database: {e}")))?;

        let mut write_options = WriteOptions::default();
        // Proof history writes must be durable once the ExEx emits progress. Keep this hard-coded
        // rather than threading it through runtime tuning options.
        write_options.set_sync(true);

        Ok(Self {
            db: Arc::new(db),
            write_options,
            append_lock: Mutex::new(()),
            prune_lock: Mutex::new(()),
            history_gate: RwLock::new(()),
        })
    }

    /// Builds top-level `RocksDB` database options from storage tuning settings.
    pub fn db_options(
        block_cache: &Cache,
        storage_options: RocksdbProofsStorageOptions,
    ) -> Options {
        let table_options = Self::table_options(block_cache);
        let mut options = Options::default();
        options.set_block_based_table_factory(&table_options);
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        options.set_max_background_jobs(storage_options.max_background_jobs);
        options.set_max_subcompactions(storage_options.max_subcompactions);
        options.set_bytes_per_sync(storage_options.bytes_per_sync);
        options.set_compaction_readahead_size(storage_options.compaction_readahead_size);
        options.set_compaction_pri(CompactionPri::MinOverlappingRatio);
        options.set_max_open_files(DEFAULT_MAX_OPEN_FILES);
        options.set_max_total_wal_size(Self::max_total_wal_size(storage_options));
        options.set_use_direct_io_for_flush_and_compaction(
            storage_options.use_direct_io_for_flush_and_compaction,
        );
        options.set_wal_ttl_seconds(0);
        options.set_wal_size_limit_mb(0);
        options
    }

    /// Returns the configured maximum total WAL size for all column families.
    pub fn max_total_wal_size(storage_options: RocksdbProofsStorageOptions) -> u64 {
        storage_options.max_total_wal_size(Self::column_families().len())
    }

    /// Builds shared block-based table options for non-history column families.
    pub fn table_options(block_cache: &Cache) -> BlockBasedOptions {
        let mut table_options = BlockBasedOptions::default();
        table_options.set_block_size(DEFAULT_BLOCK_SIZE);
        table_options.set_cache_index_and_filter_blocks(true);
        table_options.set_pin_l0_filter_and_index_blocks_in_cache(true);
        table_options.set_block_cache(block_cache);
        table_options
    }

    /// Builds block-based table options optimized for history column families.
    pub fn history_table_options(block_cache: &Cache) -> BlockBasedOptions {
        let mut table_options = Self::table_options(block_cache);
        table_options.set_bloom_filter(DEFAULT_BLOOM_BITS_PER_KEY, false);
        table_options.set_whole_key_filtering(true);
        table_options.set_optimize_filters_for_memory(true);
        table_options.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
        table_options.set_partition_filters(true);
        table_options.set_pin_top_level_index_and_filter(true);
        table_options
    }

    /// Builds per-column-family options for the given proofs storage table.
    pub fn cf_options(
        name: &'static str,
        block_cache: &Cache,
        storage_options: RocksdbProofsStorageOptions,
    ) -> Options {
        let history_prefix_len = Self::history_prefix_len(name);
        let table_options = if history_prefix_len.is_some() {
            Self::history_table_options(block_cache)
        } else {
            Self::table_options(block_cache)
        };
        let mut options = Options::default();
        options.set_block_based_table_factory(&table_options);
        if let Some(prefix_len) = history_prefix_len {
            options.set_prefix_extractor(SliceTransform::create_fixed_prefix(prefix_len));
        }
        options.set_level_compaction_dynamic_level_bytes(true);
        options.set_level_zero_file_num_compaction_trigger(
            storage_options.level_zero_file_num_compaction_trigger,
        );
        options.set_level_zero_slowdown_writes_trigger(
            storage_options.level_zero_slowdown_writes_trigger,
        );
        options.set_level_zero_stop_writes_trigger(storage_options.level_zero_stop_writes_trigger);
        options.set_max_write_buffer_number(storage_options.max_write_buffer_number);
        options.set_target_file_size_base(storage_options.target_file_size_base);
        options.set_write_buffer_size(storage_options.write_buffer_size);
        if name == <ProofWindow as Table>::NAME {
            options.set_compression_type(DBCompressionType::None);
            options.set_bottommost_compression_type(DBCompressionType::None);
        } else {
            options.set_compression_type(DBCompressionType::Lz4);
            options.set_bottommost_compression_type(DBCompressionType::Lz4);
        }
        options
    }

    /// Returns the fixed history-key prefix length for a history table name.
    pub fn history_prefix_len(name: &'static str) -> Option<usize> {
        match name {
            <AccountTrieHistory as Table>::NAME => {
                Some(<AccountTrieHistory as RocksDbHistoryTable>::KEY_LEN)
            }
            <StorageTrieHistory as Table>::NAME => {
                Some(<StorageTrieHistory as RocksDbHistoryTable>::KEY_LEN)
            }
            <HashedAccountHistory as Table>::NAME => {
                Some(<HashedAccountHistory as RocksDbHistoryTable>::KEY_LEN)
            }
            <HashedStorageHistory as Table>::NAME => {
                Some(<HashedStorageHistory as RocksDbHistoryTable>::KEY_LEN)
            }
            _ => None,
        }
    }

    /// Returns the set of proofs storage column families.
    pub const fn column_families() -> [&'static str; 6] {
        [
            <AccountTrieHistory as Table>::NAME,
            <StorageTrieHistory as Table>::NAME,
            <HashedAccountHistory as Table>::NAME,
            <HashedStorageHistory as Table>::NAME,
            <ProofWindow as Table>::NAME,
            <BlockChangeSet as Table>::NAME,
        ]
    }

    /// Returns a column family handle for the provided table name.
    pub fn cf(&self, name: &'static str) -> BaseProofsStorageResult<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| DatabaseError::Other(format!("missing RocksDB column family {name}")))
            .map_err(Into::into)
    }

    /// Encodes and writes one table row into the provided write batch.
    pub fn put_table<T: Table>(
        &self,
        batch: &mut WriteBatch,
        key: T::Key,
        value: &T::Value,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(T::NAME)?;
        batch.put_cf(&cf, encode_table_key::<T>(key), encode_table_value::<T>(value));
        Ok(())
    }

    /// Reads one table row from the live database.
    pub fn get_table<T: Table>(&self, key: T::Key) -> BaseProofsStorageResult<Option<T::Value>> {
        let cf = self.cf(T::NAME)?;
        self.db
            .get_cf(&cf, encode_table_key::<T>(key))
            .map_err(rocksdb_error)?
            .map(|value| T::Value::decompress(&value).map_err(Into::into))
            .transpose()
    }

    /// Reads one table row from the provided read snapshot.
    pub fn get_table_from_snapshot<T: Table>(
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

    /// Persists versioned history rows for a block and returns written logical keys.
    pub fn persist_history_batch<T, I, V>(
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
                    encode_history_key::<T>(&key, value.block_number)?,
                    encode_table_value::<T>(&value),
                );
            }
            return Ok(keys);
        }

        for (key, value) in pairs {
            batch.delete_cf(&cf, encode_history_key::<T>(&key, 0)?);
            if value.value.0.is_some() {
                batch.put_cf(
                    &cf,
                    encode_history_key::<T>(&key, 0)?,
                    encode_table_value::<T>(&value),
                );
            }
        }

        Ok(keys)
    }

    /// Deletes dup-sorted history entries identified by `(key, block_number)` pairs.
    pub fn delete_dup_sorted<T, I, V>(
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
            batch.delete_cf(&cf, encode_history_key::<T>(&key, block_number)?);
        }
        Ok(())
    }

    /// Collects raw history keys older than each survivor version to prune.
    pub fn collect_history_preceding_deletes<T, V>(
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
        let started = Instant::now();
        let cutoff_items_len = cutoff_items.len();
        info!(
            target: "trie::pruner",
            table = T::NAME,
            cutoff_items = cutoff_items_len,
            "Collecting RocksDB proof storage prune deletes",
        );
        if cutoff_items.is_empty() {
            info!(
                target: "trie::pruner",
                table = T::NAME,
                cutoff_items = cutoff_items_len,
                deletes = 0usize,
                elapsed = ?started.elapsed(),
                "Collected RocksDB proof storage prune deletes",
            );
            return Ok(Vec::new());
        }

        let cf = self.cf(T::NAME)?;
        let mut deletes = Vec::new();

        for (key, survivor_block) in cutoff_items {
            let prefix = encode_history_key_prefix::<T>(&key)?;
            // Under the reversed block-suffix encoding, seeking at
            // `survivor_block` lands on the newest stored version V with
            // `V <= survivor_block` and then iterates DESCENDING through
            // older versions. This is strictly cheaper than the legacy
            // "start at block 0, walk up" pattern because newer-than-survivor
            // rows are skipped entirely instead of being scanned past.
            let start_key = encode_history_key::<T>(&key, survivor_block)?;
            let read_options = exact_prefix_read_options(&prefix);
            let iter = snapshot.iterator_cf_opt(
                &cf,
                read_options,
                IteratorMode::From(&start_key, Direction::Forward),
            );

            for item in iter {
                let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
                if !raw_key.starts_with(&prefix) {
                    break;
                }

                let (_, block_number) = decode_history_key::<T>(&raw_key)?;
                if block_number > survivor_block {
                    continue;
                }
                if block_number == survivor_block {
                    let value = T::Value::decompress(&raw_value)?;
                    if value.value.0.is_none() {
                        deletes.push(raw_key.to_vec());
                    }
                    continue;
                }
                deletes.push(raw_key.to_vec());
            }
        }

        info!(
            target: "trie::pruner",
            table = T::NAME,
            cutoff_items = cutoff_items_len,
            deletes = deletes.len(),
            elapsed = ?started.elapsed(),
            "Collected RocksDB proof storage prune deletes",
        );

        Ok(deletes)
    }

    /// Deletes raw encoded history keys for a specific table.
    pub fn delete_raw_history_keys<T>(
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

    /// Wipes existing key space and overlays replacement entries for one address scope.
    pub fn wipe_and_overlay<T, Next, I, K, VV, V>(
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
                encode_history_key::<T>(&db_key, block_number)?,
                encode_table_value::<T>(&db_value),
            );
            keys.push(db_key);
        }

        Ok(keys)
    }

    /// Stores trie and hashed-state updates for one block into the batch.
    pub fn store_trie_updates_for_block(
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

    /// Appends one block update, validating parent continuity against latest stored block.
    pub fn store_trie_updates_append_only(
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

    /// Appends replacement-mode updates and records the resulting block change set.
    pub fn store_replacement_trie_updates_append_only(
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

    /// Stores replacement-mode trie updates for a specific block number.
    pub fn store_replacement_trie_updates_for_block(
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

    /// Reads a proof-window block number/hash pair for the given key.
    pub fn get_block_number_hash(
        &self,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        Ok(self.get_table::<ProofWindow>(key)?.map(|value| (value.number(), *value.hash())))
    }

    /// Reads a proof-window block number/hash pair from a snapshot.
    pub fn get_block_number_hash_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        Ok(self
            .get_table_from_snapshot::<ProofWindow>(snapshot, key)?
            .map(|value| (value.number(), *value.hash())))
    }

    /// Returns the latest known block number/hash, falling back to earliest when needed.
    pub fn get_latest_block_number_hash(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let block = self.get_block_number_hash(ProofWindowKey::LatestBlock)?;
        if block.is_some() {
            return Ok(block);
        }

        self.get_block_number_hash(ProofWindowKey::EarliestBlock)
    }

    /// Returns both earliest and latest retained proof-window bounds.
    pub fn get_proof_window(&self) -> BaseProofsStorageResult<Option<ProofWindowValue>> {
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

    /// Writes one proof-window block number/hash value into the batch.
    pub fn put_proof_window(
        &self,
        batch: &mut WriteBatch,
        key: ProofWindowKey,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        self.put_table::<ProofWindow>(batch, key, &BlockNumberHash::new(block_number, hash))
    }

    /// Sets the earliest retained block number/hash under the history write lock.
    pub fn set_earliest_block_number_hash(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let _guard = self.history_gate.write();
        self.set_earliest_block_number_hash_unlocked(block_number, hash)
    }

    /// Sets the earliest retained block number/hash without taking the history lock.
    pub fn set_earliest_block_number_hash_unlocked(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let mut batch = WriteBatch::default();
        self.put_proof_window(&mut batch, ProofWindowKey::EarliestBlock, block_number, hash)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    /// Computes survivor keys and prune metadata up to the target block.
    pub fn calculate_prune_plan(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        target_block: u64,
    ) -> BaseProofsStorageResult<Option<RocksdbPrunePlan>> {
        let started = Instant::now();
        info!(
            target: "trie::pruner",
            target_block,
            "Calculating RocksDB proof storage prune plan",
        );
        let Some((earliest, earliest_hash)) =
            self.get_block_number_hash_from_snapshot(snapshot, ProofWindowKey::EarliestBlock)?
        else {
            info!(
                target: "trie::pruner",
                target_block,
                elapsed = ?started.elapsed(),
                "Skipped RocksDB proof storage prune plan because earliest block is missing",
            );
            return Ok(None);
        };

        if earliest >= target_block {
            info!(
                target: "trie::pruner",
                earliest_block = earliest,
                target_block,
                elapsed = ?started.elapsed(),
                "Skipped RocksDB proof storage prune plan because target is not newer",
            );
            return Ok(None);
        }

        let mut acc_candidates: HashMap<StoredNibbles, u64> = HashMap::default();
        let mut storage_candidates: HashMap<StorageTrieKey, u64> = HashMap::default();
        let mut hashed_acc_candidates: HashMap<B256, u64> = HashMap::default();
        let mut hashed_storage_candidates: HashMap<HashedStorageKey, u64> = HashMap::default();

        for result in self
            .iter_change_sets_from_snapshot(snapshot, (earliest.saturating_add(1))..=target_block)?
        {
            let (block_number, change_set) = result?;
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

        let plan = RocksdbPrunePlan {
            earliest_block: earliest,
            earliest_hash,
            acc_survivors: flatten_and_sort(acc_candidates),
            storage_survivors: flatten_and_sort(storage_candidates),
            hashed_acc_survivors: flatten_and_sort(hashed_acc_candidates),
            hashed_storage_survivors: flatten_and_sort(hashed_storage_candidates),
        };

        info!(
            target: "trie::pruner",
            earliest_block = plan.earliest_block,
            target_block,
            account_trie_survivors = plan.acc_survivors.len(),
            storage_trie_survivors = plan.storage_survivors.len(),
            hashed_account_survivors = plan.hashed_acc_survivors.len(),
            hashed_storage_survivors = plan.hashed_storage_survivors.len(),
            total_survivors = plan.total_survivors(),
            elapsed = ?started.elapsed(),
            "Calculated RocksDB proof storage prune plan",
        );

        Ok(Some(plan))
    }

    /// Collects all history rows referenced by change sets in a block range.
    pub fn collect_history_ranged(
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

    /// Deletes all history rows and change sets contained in a prepared range batch.
    pub fn delete_history_ranged(
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

    /// Iterates decoded block change sets in ascending block order for the range.
    pub fn iter_change_sets(
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

    /// Iterates decoded block change sets from a snapshot for the range.
    pub fn iter_change_sets_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        block_range: impl RangeBounds<u64> + 'static,
    ) -> BaseProofsStorageResult<impl Iterator<Item = BaseProofsStorageResult<(u64, ChangeSet)>>>
    {
        let cf = self.cf(<BlockChangeSet as Table>::NAME)?;
        let start = range_start(&block_range);
        let start_key = encode_block_number(start);

        let iter = snapshot
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward))
            .map(move |item| -> Result<Option<(u64, ChangeSet)>, BaseProofsStorageError> {
                let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
                let block_number = decode_block_number(&raw_key)?;
                if !block_range.contains(&block_number) {
                    // break
                    return Ok(None);
                }

                Ok(Some((block_number, ChangeSet::decompress(&raw_value)?)))
            })
            .map_while(|item| match item {
                Err(e) => Some(Err(e)),
                Ok(None) => None,
                Ok(Some(item)) => Some(Ok(item)),
            });

        Ok(iter)
    }

    /// Prepares a prune batch for advancing the earliest retained block.
    pub fn prepare_prune(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<Option<RocksdbPreparedPrune>> {
        let started = Instant::now();
        let requested_target = new_earliest_block_ref.block.number;
        info!(
            target: "trie::pruner",
            requested_target,
            "Preparing RocksDB proof storage prune",
        );
        let snapshot = self.db.snapshot();
        let Some((earliest_block, earliest_hash)) =
            self.get_block_number_hash_from_snapshot(&snapshot, ProofWindowKey::EarliestBlock)?
        else {
            info!(
                target: "trie::pruner",
                requested_target,
                elapsed = ?started.elapsed(),
                "Skipped RocksDB proof storage prune because earliest block is missing",
            );
            return Ok(None);
        };

        let (latest_block, latest_hash) = self
            .get_block_number_hash_from_snapshot(&snapshot, ProofWindowKey::LatestBlock)?
            .unwrap_or((earliest_block, earliest_hash));

        // Bound the prune to rows visible in this snapshot. Appends may commit while pruning, but
        // they must always land above this effective target.
        let (target_block, target_hash) = if requested_target > latest_block {
            (latest_block, latest_hash)
        } else {
            (requested_target, new_earliest_block_ref.block.hash)
        };

        info!(
            target: "trie::pruner",
            earliest_block,
            latest_block,
            requested_target,
            target_block,
            clamped_to_latest = requested_target > latest_block,
            "Calculated RocksDB proof storage prune target",
        );

        if earliest_block >= target_block {
            info!(
                target: "trie::pruner",
                earliest_block,
                target_block,
                elapsed = ?started.elapsed(),
                "Skipped RocksDB proof storage prune because target is not newer",
            );
            return Ok(None);
        }

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

        info!(
            target: "trie::pruner",
            expected_earliest_block = plan.earliest_block,
            target_block,
            account_trie_deletes = account_trie.len(),
            storage_trie_deletes = storage_trie.len(),
            hashed_account_deletes = hashed_account.len(),
            hashed_storage_deletes = hashed_storage.len(),
            total_deletes = counts.account_trie_updates_written_total
                + counts.storage_trie_updates_written_total
                + counts.hashed_accounts_written_total
                + counts.hashed_storages_written_total,
            elapsed = ?started.elapsed(),
            "Prepared RocksDB proof storage prune",
        );

        Ok(Some(RocksdbPreparedPrune {
            expected_earliest_block: plan.earliest_block,
            expected_earliest_hash: plan.earliest_hash,
            target_block,
            target_hash,
            deletes: RocksdbPreparedHistoryDeletes {
                account_trie,
                storage_trie,
                hashed_account,
                hashed_storage,
            },
            counts,
        }))
    }

    /// Commits a previously prepared prune batch if the earliest anchor still matches.
    ///
    /// Callers must hold [`Self::prune_lock`] across both [`Self::prepare_prune`] and this
    /// method so that `EarliestBlock` cannot change between preparation, the staleness
    /// re-check below, and the batch write. [`Self::prune_earliest_state`] is the orchestrator
    /// that owns this lock for the whole prepare/commit sequence.
    pub fn commit_prepared_prune(
        &self,
        prepared: RocksdbPreparedPrune,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let started = Instant::now();
        let RocksdbPreparedPrune {
            expected_earliest_block,
            expected_earliest_hash,
            target_block,
            target_hash,
            deletes,
            counts,
        } = prepared;
        info!(
            target: "trie::pruner",
            expected_earliest_block,
            target_block,
            account_trie_deletes = deletes.account_trie.len(),
            storage_trie_deletes = deletes.storage_trie.len(),
            hashed_account_deletes = deletes.hashed_account.len(),
            hashed_storage_deletes = deletes.hashed_storage.len(),
            total_deletes = deletes.total(),
            "Committing RocksDB proof storage prune",
        );
        let expected_earliest = Some((expected_earliest_block, expected_earliest_hash));
        let current_earliest = self.get_block_number_hash(ProofWindowKey::EarliestBlock)?;
        if current_earliest != expected_earliest {
            info!(
                target: "trie::pruner",
                current_earliest = ?current_earliest,
                expected_earliest = ?expected_earliest,
                target_block,
                elapsed = ?started.elapsed(),
                "skipping stale prune plan"
            );
            return Ok(WriteCounts::default());
        }

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
            target_hash,
        )?;

        info!(
            target: "trie::pruner",
            expected_earliest_block,
            target_block,
            elapsed = ?started.elapsed(),
            "Writing RocksDB proof storage prune batch",
        );
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        info!(
            target: "trie::pruner",
            expected_earliest_block,
            target_block,
            account_trie_deletes = counts.account_trie_updates_written_total,
            storage_trie_deletes = counts.storage_trie_updates_written_total,
            hashed_account_deletes = counts.hashed_accounts_written_total,
            hashed_storage_deletes = counts.hashed_storages_written_total,
            total_deletes = counts.account_trie_updates_written_total
                + counts.storage_trie_updates_written_total
                + counts.hashed_accounts_written_total
                + counts.hashed_storages_written_total,
            elapsed = ?started.elapsed(),
            "Committed RocksDB proof storage prune",
        );
        Ok(counts)
    }

    /// Returns the initial-state anchor block if one has been stored.
    pub fn get_initial_state_anchor(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        Ok(self.get_table::<ProofWindow>(ProofWindowKey::InitialStateAnchor)?.map(Into::into))
    }

    /// Returns the lexicographically greatest logical history key in a table.
    pub fn get_latest_history_key<T>(&self) -> BaseProofsStorageResult<Option<T::Key>>
    where
        T: RocksDbHistoryTable,
    {
        let cf = self.cf(T::NAME)?;
        // This returns the lexicographically LARGEST user-key prefix that has
        // any history entry, used as a resume cursor during initial state
        // population. The block-suffix encoding (forward or reversed) is
        // irrelevant here: only the tiebreak among rows sharing a user-key
        // prefix changes, so `IteratorMode::End` still lands on a row whose
        // user-key prefix is the maximum.
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
        unimplemented!("read path not yet implemented")
    }

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_block_number_hash(ProofWindowKey::EarliestBlock)
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_latest_block_number_hash()
    }

    fn storage_trie_cursor<'tx>(
        &'tx self,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        unimplemented!("read path not yet implemented")
    }

    fn account_trie_cursor<'tx>(
        &'tx self,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        unimplemented!("read path not yet implemented")
    }

    fn storage_hashed_cursor<'tx>(
        &'tx self,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>> {
        unimplemented!("read path not yet implemented")
    }

    fn account_hashed_cursor<'tx>(
        &'tx self,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        unimplemented!("read path not yet implemented")
    }

    fn storage_trie_cursor_with_tx<'tx, 'db>(
        &self,
        _tx: &'tx Self::Tx<'db>,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        unimplemented!("read path not yet implemented")
    }

    fn account_trie_cursor_with_tx<'tx, 'db>(
        &self,
        _tx: &'tx Self::Tx<'db>,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        unimplemented!("read path not yet implemented")
    }

    fn storage_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        _tx: &'tx Self::Tx<'db>,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        unimplemented!("read path not yet implemented")
    }

    fn account_hashed_cursor_with_tx<'tx, 'db>(
        &self,
        _tx: &'tx Self::Tx<'db>,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'tx>>
    where
        Self: 'db,
        'db: 'tx,
    {
        unimplemented!("read path not yet implemented")
    }

    fn store_trie_updates(
        &self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let _append_guard = self.append_lock.lock();
        let _history_guard = self.history_gate.read();
        let mut batch = WriteBatch::default();
        let counts =
            self.store_trie_updates_append_only(&mut batch, block_ref, block_state_diff)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(counts)
    }

    fn fetch_trie_updates(&self, _block_number: u64) -> BaseProofsStorageResult<BlockStateDiff> {
        unimplemented!("read path not yet implemented")
    }

    fn prune_earliest_state(
        &self,
        new_earliest_block_ref: BlockWithParent,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let started = Instant::now();
        info!(
            target: "trie::pruner",
            target_block = new_earliest_block_ref.block.number,
            "Acquiring RocksDB proof storage prune locks",
        );
        let _prune_guard = self.prune_lock.lock();
        let _history_guard = self.history_gate.read();
        info!(
            target: "trie::pruner",
            target_block = new_earliest_block_ref.block.number,
            elapsed = ?started.elapsed(),
            "Acquired RocksDB proof storage prune locks",
        );
        let Some(prepared) = self.prepare_prune(new_earliest_block_ref)? else {
            info!(
                target: "trie::pruner",
                target_block = new_earliest_block_ref.block.number,
                elapsed = ?started.elapsed(),
                "No RocksDB proof storage prune work prepared",
            );
            return Ok(WriteCounts::default());
        };

        let counts = self.commit_prepared_prune(prepared)?;
        info!(
            target: "trie::pruner",
            target_block = new_earliest_block_ref.block.number,
            elapsed = ?started.elapsed(),
            "Finished RocksDB proof storage prune request",
        );
        Ok(counts)
    }

    fn unwind_history(&self, to: BlockWithParent) -> BaseProofsStorageResult<()> {
        let _guard = self.history_gate.write();
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

        // Keep collection and deletion under the same exclusive history gate so another history
        // rewrite cannot change the proof window or history rows between choosing keys and
        // committing the batch.
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
        _latest_common_block: BlockNumHash,
        _blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
    ) -> BaseProofsStorageResult<()> {
        unimplemented!("read path not yet implemented")
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
        let _guard = self.history_gate.write();
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
        let _guard = self.history_gate.write();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<AccountTrieHistory, _, _>(&mut batch, 0, account_nodes, true)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn store_storage_branches(
        &self,
        hashed_address: B256,
        storage_nodes: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> BaseProofsStorageResult<()> {
        self.store_storage_branches_bulk(vec![(hashed_address, storage_nodes)])
    }

    fn store_storage_branches_bulk(
        &self,
        entries: Vec<(B256, Vec<(Nibbles, Option<BranchNodeCompact>)>)>,
    ) -> BaseProofsStorageResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = self.history_gate.write();
        let mut batch = WriteBatch::default();
        for (hashed_address, mut storage_nodes) in entries {
            if storage_nodes.is_empty() {
                continue;
            }
            storage_nodes.sort_by_key(|(key, _)| *key);
            self.persist_history_batch::<StorageTrieHistory, _, _>(
                &mut batch,
                0,
                storage_nodes.into_iter().map(|(path, node)| (hashed_address, path, node)),
                true,
            )?;
        }
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
        let _guard = self.history_gate.write();
        let mut batch = WriteBatch::default();
        self.persist_history_batch::<HashedAccountHistory, _, _>(&mut batch, 0, accounts, true)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        storages: Vec<(B256, U256)>,
    ) -> BaseProofsStorageResult<()> {
        self.store_hashed_storages_bulk(vec![(hashed_address, storages)])
    }

    fn store_hashed_storages_bulk(
        &self,
        entries: Vec<(B256, Vec<(B256, U256)>)>,
    ) -> BaseProofsStorageResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = self.history_gate.write();
        let mut batch = WriteBatch::default();
        for (hashed_address, mut storages) in entries {
            if storages.is_empty() {
                continue;
            }
            storages.sort_by_key(|(key, _)| *key);
            self.persist_history_batch::<HashedStorageHistory, _, _>(
                &mut batch,
                0,
                storages
                    .into_iter()
                    .map(|(key, value)| (hashed_address, key, Some(StorageValue(value)))),
                true,
            )?;
        }
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let _guard = self.history_gate.write();
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        self.set_earliest_block_number_hash_unlocked(anchor.number, anchor.hash)?;
        Ok(anchor)
    }
}

/// Batch session for [`RocksdbProofsStorage`].
///
/// Unlike the MDBX implementation, `RocksDB` does not expose a transaction cursor readable
/// mid-session; each `store_trie_updates` call commits immediately via a write batch. To
/// avoid the per-cursor cost of `db.snapshot()` (which pins SST files against compaction
/// and is expensive enough at the thousands-per-block scale to stall sync), the session
/// holds ONE snapshot and reuses it across all cursor reads. The snapshot is refreshed
/// after each `store_trie_updates` so subsequent block reads observe the prior commit.
#[derive(Debug)]
pub struct RocksdbBatchSession<'a> {
    /// Backend storage handle the session writes through.
    pub storage: &'a RocksdbProofsStorage,
    /// Shared read snapshot reused across all cursor reads in the session.
    pub snapshot: Arc<RocksdbReadSnapshot<'a>>,
}

impl<'a> RocksdbBatchSession<'a> {
    /// Opens a new batch session holding a single shared read snapshot.
    pub fn new(storage: &'a RocksdbProofsStorage) -> Self {
        let snapshot = Arc::new(RocksdbReadSnapshot::new(storage.db.as_ref()));
        Self { storage, snapshot }
    }
}

impl BaseProofsBatchSession for RocksdbBatchSession<'_> {
    type StorageTrieCursor<'a>
        = RocksdbTrieCursor<'a, StorageTrieHistory>
    where
        Self: 'a;
    type AccountTrieCursor<'a>
        = RocksdbTrieCursor<'a, AccountTrieHistory>
    where
        Self: 'a;
    type StorageCursor<'a>
        = RocksdbStorageCursor<'a>
    where
        Self: 'a;
    type AccountHashedCursor<'a>
        = RocksdbAccountCursor<'a>
    where
        Self: 'a;

    fn get_earliest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.get_earliest_block_number()
    }

    fn get_latest_block_number(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.storage.get_latest_block_number()
    }

    fn storage_trie_cursor(
        &self,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageTrieCursor<'_>> {
        unimplemented!("read path not yet implemented")
    }

    fn account_trie_cursor(
        &self,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'_>> {
        unimplemented!("read path not yet implemented")
    }

    fn storage_hashed_cursor(
        &self,
        _hashed_address: B256,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::StorageCursor<'_>> {
        unimplemented!("read path not yet implemented")
    }

    fn account_hashed_cursor(
        &self,
        _max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountHashedCursor<'_>> {
        unimplemented!("read path not yet implemented")
    }

    fn store_trie_updates(
        &mut self,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let counts = self.storage.store_trie_updates(block_ref, block_state_diff)?;
        // Refresh the snapshot so the next block's cursor reads observe this commit.
        self.snapshot = Arc::new(RocksdbReadSnapshot::new(self.storage.db.as_ref()));
        Ok(counts)
    }
}

impl BaseProofsBatchStore for RocksdbProofsStorage {
    type BatchSession<'a>
        = RocksdbBatchSession<'a>
    where
        Self: 'a;

    fn with_batch_session<R, F>(&self, f: F) -> BaseProofsStorageResult<R>
    where
        F: FnOnce(&mut Self::BatchSession<'_>) -> BaseProofsStorageResult<R>,
    {
        let mut session = RocksdbBatchSession::new(self);
        f(&mut session)
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
    T::Key: Clone + Default + Ord,
    T::Value: Decompress,
{
    /// Creates a cursor over a `RocksDB` history column family.
    pub fn new(_db: &'db RocksDb, _max_block_number: u64) -> Self {
        unimplemented!("read path not yet implemented")
    }

    /// Creates a versioned cursor that reads from a shared snapshot.
    pub fn new_with_snapshot(
        _snapshot: Arc<RocksdbReadSnapshot<'db>>,
        _max_block_number: u64,
    ) -> Self {
        unimplemented!("read path not yet implemented")
    }

    /// Returns the column family handle for this cursor's history table.
    pub fn cf(&self) -> Result<Arc<BoundColumnFamily<'_>>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns encoded lower and upper bounds for an exact key lookup.
    pub fn exact_lookup_bounds(&self, _key: &T::Key) -> Result<(Vec<u8>, Vec<u8>), DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns the latest visible version for the provided logical key.
    pub fn latest_version_for_key(&self, _key: T::Key) -> RocksDbLatestVersionResult<T> {
        unimplemented!("read path not yet implemented")
    }

    /// Seeks to an exact key and returns its decoded visible value.
    pub fn seek_exact(&self, _key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Seeks to the first key at or after the provided start key.
    pub fn seek(&self, _start_key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Advances the cursor to the next decoded row.
    pub fn next(&self) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns the next live row at or after the given key.
    pub fn next_live_from(&self, _key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns the next live row strictly after the given key.
    pub fn next_live_after(&self, _key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Seeks using an explicit encoded prefix bound.
    pub fn seek_with_prefix(
        &self,
        _key: T::Key,
        _prefix: Vec<u8>,
    ) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Advances to the next row constrained to the provided prefix.
    pub fn next_with_prefix(&self, _prefix: Vec<u8>) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns the next candidate row while optionally skipping tombstoned values.
    pub fn next_live_candidate(
        &self,
        _key: T::Key,
        _exclusive: bool,
        _read_options: ReadOptions,
    ) -> Result<Option<(T::Key, V)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    /// Returns `true` when the cursor currently points at a key.
    pub const fn is_positioned(&self) -> bool {
        self.current_key.is_some()
    }
}

impl<'db> RocksdbTrieCursor<'db, AccountTrieHistory> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(_db: &'db RocksDb, _max_block_number: u64, _hashed_address: Option<B256>) -> Self {
        unimplemented!("read path not yet implemented")
    }

    /// Creates a `RocksDB` trie cursor over a shared read snapshot.
    /// Creates a `RocksDB` storage cursor over a shared read snapshot.
    pub fn new_with_snapshot(
        _snapshot: Arc<RocksdbReadSnapshot<'db>>,
        _max_block_number: u64,
        _hashed_address: Option<B256>,
    ) -> Self {
        unimplemented!("read path not yet implemented")
    }
}

impl<'db> RocksdbTrieCursor<'db, StorageTrieHistory> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(_db: &'db RocksDb, _max_block_number: u64, _hashed_address: Option<B256>) -> Self {
        unimplemented!("read path not yet implemented")
    }

    /// Creates a `RocksDB` trie cursor over a shared read snapshot.
    /// Creates a `RocksDB` account cursor over a shared read snapshot.
    pub fn new_with_snapshot(
        _snapshot: Arc<RocksdbReadSnapshot<'db>>,
        _max_block_number: u64,
        _hashed_address: Option<B256>,
    ) -> Self {
        unimplemented!("read path not yet implemented")
    }
}

impl TrieCursor for RocksdbTrieCursor<'_, AccountTrieHistory> {
    fn seek_exact(
        &mut self,
        _path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn seek(
        &mut self,
        _path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn reset(&mut self) {
        unimplemented!("read path not yet implemented")
    }
}

impl TrieCursor for RocksdbTrieCursor<'_, StorageTrieHistory> {
    fn seek_exact(
        &mut self,
        _path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn seek(
        &mut self,
        _path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn reset(&mut self) {
        unimplemented!("read path not yet implemented")
    }
}

impl TrieStorageCursor for RocksdbTrieCursor<'_, StorageTrieHistory> {
    fn set_hashed_address(&mut self, _hashed_address: B256) {
        unimplemented!("read path not yet implemented")
    }
}

impl<'db> RocksdbStorageCursor<'db> {
    /// Creates a `RocksDB` storage cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: B256) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number), hashed_address }
    }

    /// Creates a `RocksDB` storage cursor over a shared read snapshot.
    pub fn new_with_snapshot(
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

    fn seek(&mut self, _key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn reset(&mut self) {
        unimplemented!("read path not yet implemented")
    }
}

impl HashedStorageCursor for RocksdbStorageCursor<'_> {
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn set_hashed_address(&mut self, _hashed_address: B256) {
        unimplemented!("read path not yet implemented")
    }
}

impl<'db> RocksdbAccountCursor<'db> {
    /// Creates a `RocksDB` account cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64) -> Self {
        Self { inner: RocksdbVersionedCursor::new(db, max_block_number) }
    }

    /// Creates a `RocksDB` account cursor over a shared read snapshot.
    pub fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
    ) -> Self {
        Self { inner: RocksdbVersionedCursor::new_with_snapshot(snapshot, max_block_number) }
    }
}

impl HashedCursor for RocksdbAccountCursor<'_> {
    type Value = Account;

    fn seek(&mut self, _key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        unimplemented!("read path not yet implemented")
    }

    fn reset(&mut self) {
        unimplemented!("read path not yet implemented")
    }
}

/// Converts a `RocksDB` crate error into [`DatabaseError`].
fn rocksdb_error(error: rocksdb::Error) -> DatabaseError {
    DatabaseError::Other(error.to_string())
}

/// Encodes a table key into bytes suitable for `RocksDB` storage.
fn encode_table_key<T: Table>(key: T::Key) -> Vec<u8> {
    key.encode().as_ref().to_vec()
}

fn encode_table_value<T: Table>(value: &T::Value) -> Vec<u8> {
    let mut encoded = <T::Value as Compress>::Compressed::default();
    value.compress_to_buf(&mut encoded);
    encoded.into()
}

fn encode_history_key<T>(key: &T::Key, block_number: u64) -> Result<Vec<u8>, DatabaseError>
where
    T: RocksDbHistoryTable,
{
    let mut encoded = encode_history_key_prefix::<T>(key)?;
    encoded.extend_from_slice(&encode_history_block_suffix(block_number));
    Ok(encoded)
}

fn encode_history_key_prefix<T>(key: &T::Key) -> Result<Vec<u8>, DatabaseError>
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
    let block_number = decode_history_block_suffix(&raw_key[split..])?;
    Ok((key, block_number))
}

/// Reads one exact history value for a key at a specific block number.
#[expect(dead_code, reason = "used by the RocksDB read/cursor path implemented in a later PR")]
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
        .get_cf(&cf, encode_history_key::<T>(&key, block_number)?)
        .map_err(rocksdb_error)?
        .map(|value| T::Value::decompress(&value).map_err(Into::into))
        .transpose()
}

impl RocksDbHistoryTable for AccountTrieHistory {
    const KEY_LEN: usize = PACKED_NIBBLES_KEY_LEN;

    fn encode_history_key_prefix(key: &Self::Key) -> Result<Vec<u8>, DatabaseError> {
        Ok(encode_packed_nibbles(&key.0)?.to_vec())
    }

    fn decode_history_key_prefix(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        decode_packed_nibbles(raw_key).map(StoredNibbles)
    }
}

impl RocksDbHistoryTable for StorageTrieHistory {
    const KEY_LEN: usize = HASH_KEY_LEN + PACKED_NIBBLES_KEY_LEN;

    fn encode_history_key_prefix(key: &Self::Key) -> Result<Vec<u8>, DatabaseError> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(&encode_packed_nibbles(&key.path.0)?);
        Ok(encoded)
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

    fn encode_history_key_prefix(key: &Self::Key) -> Result<Vec<u8>, DatabaseError> {
        Ok(key.as_slice().to_vec())
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

    fn encode_history_key_prefix(key: &Self::Key) -> Result<Vec<u8>, DatabaseError> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(key.hashed_storage_key.as_slice());
        Ok(encoded)
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

/// Encodes trie nibbles into the fixed-width packed key format.
fn encode_packed_nibbles(nibbles: &Nibbles) -> Result<[u8; PACKED_NIBBLES_KEY_LEN], DatabaseError> {
    if nibbles.len() > 64 {
        return Err(DatabaseError::Other(format!(
            "trie path has {} nibbles, max is 64",
            nibbles.len()
        )));
    }
    let mut encoded = [0; PACKED_NIBBLES_KEY_LEN];
    nibbles.pack_to(&mut encoded[..HASH_KEY_LEN]);
    encoded[HASH_KEY_LEN] = nibbles.len() as u8;
    Ok(encoded)
}

/// Decodes trie nibbles from the fixed-width packed key format.
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

/// Encodes a block number as a big-endian key.
const fn encode_block_number(block_number: u64) -> [u8; 8] {
    block_number.to_be_bytes()
}

/// Decodes a big-endian block-number key.
fn decode_block_number(raw_key: &[u8]) -> Result<u64, DatabaseError> {
    if raw_key.len() != 8 {
        return Err(DatabaseError::Decode);
    }
    Ok(u64::from_be_bytes(raw_key.try_into().map_err(|_| DatabaseError::Decode)?))
}

/// Encodes the block-number suffix attached to history-table keys so that newer
/// blocks sort BEFORE older blocks under the default `BytewiseComparator`.
///
/// This is the standard "complement key" trick (`u64::MAX - block_number`) used
/// to make "find latest version at or below block N" fast on an LSM-tree: a
/// forward `seek(target)` followed by `next()` lands on the answer rather than
/// requiring the much slower `seek_for_prev()` / `prev()` path.
///
/// Used ONLY for history-table key suffixes (`AccountTrieHistory`,
/// `StorageTrieHistory`, `HashedAccountHistory`, `HashedStorageHistory`).
/// `BlockChangeSet` keys remain plain big-endian (forward range scans).
const fn encode_history_block_suffix(block_number: u64) -> [u8; 8] {
    (u64::MAX - block_number).to_be_bytes()
}

/// Inverse of [`encode_history_block_suffix`].
fn decode_history_block_suffix(raw_suffix: &[u8]) -> Result<u64, DatabaseError> {
    if raw_suffix.len() != BLOCK_NUMBER_KEY_LEN {
        return Err(DatabaseError::Decode);
    }
    let complement = u64::from_be_bytes(raw_suffix.try_into().map_err(|_| DatabaseError::Decode)?);
    Ok(u64::MAX - complement)
}

/// Returns the first included block number in the provided range bounds.
fn range_start(range: &impl RangeBounds<u64>) -> u64 {
    match range.start_bound() {
        Bound::Included(start) => *start,
        Bound::Excluded(start) => start.saturating_add(1),
        Bound::Unbounded => 0,
    }
}

/// Converts a map into key-sorted `(key, block_number)` pairs.
fn flatten_and_sort<K: Ord>(map: HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut values: Vec<_> = map.into_iter().collect();
    values.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    values
}

/// Builds `RocksDB` read options bounded to a key prefix range.
fn prefix_read_options(prefix: &[u8]) -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_iterate_lower_bound(prefix.to_vec());
    if let Some(upper_bound) = prefix_upper_bound(prefix) {
        read_options.set_iterate_upper_bound(upper_bound);
    }
    // The provided `prefix` is shorter than the CF-configured fixed_prefix
    // extractor (which spans the full key prefix incl. inner sub-key bytes).
    // Without total_order_seek=true, the bloom filter would skip SST blocks
    // whose extracted prefix doesn't match the seek key's, silently dropping
    // all entries except the one whose full CF prefix matches exactly.
    // total_order_seek bypasses the prefix-domain restriction; the explicit
    // iterate_lower_bound/iterate_upper_bound above still bound the scan.
    read_options.set_total_order_seek(true);
    read_options
}

/// Builds prefix-bounded read options that require matching start prefix.
fn exact_prefix_read_options(prefix: &[u8]) -> ReadOptions {
    let mut read_options = prefix_read_options(prefix);
    read_options.set_prefix_same_as_start(true);
    read_options
}

/// Builds read options with total-order seek enabled.
#[expect(dead_code, reason = "used by the RocksDB read/cursor path implemented in a later PR")]
fn total_order_read_options() -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_total_order_seek(true);
    read_options
}

/// Returns the encoded key prefix for a hashed address.
#[expect(dead_code, reason = "used by the RocksDB read/cursor path implemented in a later PR")]
fn hashed_address_prefix(hashed_address: B256) -> Vec<u8> {
    hashed_address.as_slice().to_vec()
}

/// Computes the lexicographic upper bound for a prefix, if one exists.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper_bound = prefix.to_vec();
    for index in (0..upper_bound.len()).rev() {
        if upper_bound[index] != u8::MAX {
            upper_bound[index] += 1;
            upper_bound.truncate(index + 1);
            return Some(upper_bound);
        }
    }

    None
}
