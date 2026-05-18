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
use reth_trie_common::{
    BranchNodeCompact, HashedPostState, Nibbles, StoredNibbles,
    updates::{StorageTrieUpdates, TrieUpdates},
};
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
const DEFAULT_BLOCK_CACHE_SIZE: usize = 1024 << 20;
const DEFAULT_BLOCK_SIZE: usize = 16 * 1024;
const DEFAULT_BYTES_PER_SYNC: u64 = 1_048_576;
const DEFAULT_COMPACTION_READAHEAD_SIZE: usize = 0;
const DEFAULT_DIRECT_IO_FOR_FLUSH_AND_COMPACTION: bool = true;
const DEFAULT_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER: i32 = 4;
const DEFAULT_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER: i32 = 20;
const DEFAULT_LEVEL_ZERO_STOP_WRITES_TRIGGER: i32 = 36;
const DEFAULT_RATE_LIMITER_FAIRNESS: i32 = 10;
const DEFAULT_RATE_LIMITER_REFILL_PERIOD_US: i64 = 100_000;
const DEFAULT_MAX_BACKGROUND_JOBS: i32 = 4;
const DEFAULT_MAX_SUBCOMPACTIONS: u32 = 1;
const DEFAULT_MAX_OPEN_FILES: i32 = -1;
const DEFAULT_MAX_WRITE_BUFFER_NUMBER: i32 = 3;
const DEFAULT_TARGET_FILE_SIZE_BASE: u64 = 256 * 1024 * 1024;
const DEFAULT_WRITE_BUFFER_SIZE: usize = 64 * 1024 * 1024;
const DEFAULT_BLOOM_BITS_PER_KEY: f64 = 10.0;

/// Compression policy for `RocksDB` proof-history column families.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RocksdbProofsCompression {
    /// Disable compression for new files.
    None,
    /// Use LZ4 compression for new files.
    #[default]
    Lz4,
    /// Use ZSTD compression for new files.
    Zstd,
}

impl RocksdbProofsCompression {
    /// Returns the corresponding `RocksDB` compression type.
    pub const fn db_compression_type(self) -> DBCompressionType {
        match self {
            Self::None => DBCompressionType::None,
            Self::Lz4 => DBCompressionType::Lz4,
            Self::Zstd => DBCompressionType::Zstd,
        }
    }
}

/// Options for opening [`RocksdbProofsStorage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RocksdbProofsStorageOptions {
    /// Compression for non-bottommost proof-history files.
    pub compression: RocksdbProofsCompression,
    /// Compression for bottommost proof-history files.
    pub bottommost_compression: RocksdbProofsCompression,
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
    /// Optional `RocksDB` write rate limit in bytes per second.
    pub rate_limit_bytes_per_sec: Option<i64>,
    /// Rate limiter refill period in microseconds.
    pub rate_limiter_refill_period_us: i64,
    /// Rate limiter fairness.
    pub rate_limiter_fairness: i32,
}

impl Default for RocksdbProofsStorageOptions {
    fn default() -> Self {
        Self {
            compression: RocksdbProofsCompression::Lz4,
            bottommost_compression: RocksdbProofsCompression::Lz4,
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
            rate_limit_bytes_per_sec: None,
            rate_limiter_refill_period_us: DEFAULT_RATE_LIMITER_REFILL_PERIOD_US,
            rate_limiter_fairness: DEFAULT_RATE_LIMITER_FAIRNESS,
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
    // Serializes append-only writers that read LatestBlock before writing LatestBlock + 1.
    append_lock: Mutex<()>,
    // Serializes prune plans that assume a stable EarliestBlock across prepare and commit.
    prune_lock: Mutex<()>,
    // Append and prune share read access. History rewrites take write access.
    history_gate: RwLock<()>,
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
struct RocksdbPreparedPrune {
    expected_earliest_block: u64,
    expected_earliest_hash: B256,
    target_block: u64,
    target_hash: B256,
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

    const fn snapshot(&self) -> &SnapshotWithThreadMode<'db, RocksDb> {
        &self.snapshot
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
        // Proof history is derivable from the canonical chain, so use async WAL writes for
        // throughput. RocksDB write batches still keep committed updates internally consistent.
        write_options.set_sync(false);

        Ok(Self {
            db: Arc::new(db),
            write_options,
            append_lock: Mutex::new(()),
            prune_lock: Mutex::new(()),
            history_gate: RwLock::new(()),
        })
    }

    fn db_options(block_cache: &Cache, storage_options: RocksdbProofsStorageOptions) -> Options {
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
        if let Some(rate_limit_bytes_per_sec) = storage_options.rate_limit_bytes_per_sec {
            options.set_ratelimiter(
                rate_limit_bytes_per_sec,
                storage_options.rate_limiter_refill_period_us,
                storage_options.rate_limiter_fairness,
            );
        }
        options.set_wal_ttl_seconds(0);
        options.set_wal_size_limit_mb(0);
        options
    }

    fn max_total_wal_size(storage_options: RocksdbProofsStorageOptions) -> u64 {
        storage_options.max_total_wal_size(Self::column_families().len())
    }

    fn table_options(block_cache: &Cache) -> BlockBasedOptions {
        let mut table_options = BlockBasedOptions::default();
        table_options.set_block_size(DEFAULT_BLOCK_SIZE);
        table_options.set_cache_index_and_filter_blocks(true);
        table_options.set_pin_l0_filter_and_index_blocks_in_cache(true);
        table_options.set_block_cache(block_cache);
        table_options
    }

    fn history_table_options(block_cache: &Cache) -> BlockBasedOptions {
        let mut table_options = Self::table_options(block_cache);
        table_options.set_bloom_filter(DEFAULT_BLOOM_BITS_PER_KEY, false);
        table_options.set_whole_key_filtering(true);
        table_options.set_optimize_filters_for_memory(true);
        table_options.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
        table_options.set_partition_filters(true);
        table_options.set_pin_top_level_index_and_filter(true);
        table_options
    }

    fn cf_options(
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
            options.set_compression_type(storage_options.compression.db_compression_type());
            options.set_bottommost_compression_type(
                storage_options.bottommost_compression.db_compression_type(),
            );
        }
        options
    }

    fn history_prefix_len(name: &'static str) -> Option<usize> {
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
            let prefix = encode_history_key_prefix::<T>(&key);
            let start_key = encode_history_key::<T>(&key, 0);
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
        let _guard = self.history_gate.write();
        self.set_earliest_block_number_hash_unlocked(block_number, hash)
    }

    fn set_earliest_block_number_hash_unlocked(
        &self,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
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

    fn commit_prepared_prune(
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

    fn account_by_hashed_key_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_key: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<Account>>
    where
        Self: 'db,
        'db: 'tx,
    {
        let cursor = RocksdbVersionedCursor::<HashedAccountHistory>::new_with_snapshot(
            Arc::clone(tx),
            max_block_number,
        );
        let Some(value) = cursor.latest_value_for_key(hashed_key)? else {
            return Ok(None);
        };
        Ok(value.value.0)
    }

    fn storage_by_hashed_key_with_tx<'tx, 'db>(
        &self,
        tx: &'tx Self::Tx<'db>,
        hashed_address: B256,
        hashed_storage_key: B256,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Option<U256>>
    where
        Self: 'db,
        'db: 'tx,
    {
        let cursor = RocksdbVersionedCursor::<HashedStorageHistory>::new_with_snapshot(
            Arc::clone(tx),
            max_block_number,
        );
        let key = HashedStorageKey::new(hashed_address, hashed_storage_key);
        let Some(value) = cursor.latest_value_for_key(key)? else {
            return Ok(None);
        };
        Ok(value.value.0.and_then(|value| (!value.0.is_zero()).then_some(value.0)))
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

        let _guard = self.history_gate.write();
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
        let _guard = self.history_gate.write();
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
        let _guard = self.history_gate.write();
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
        let _guard = self.history_gate.write();
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
        let _guard = self.history_gate.write();
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        self.set_earliest_block_number_hash_unlocked(anchor.number, anchor.hash)?;
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
    T::Key: Clone + Default + Ord,
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

    fn exact_lookup_bounds(&self, key: &T::Key) -> (Vec<u8>, Vec<u8>) {
        let mut target = encode_history_key_prefix::<T>(key);
        let prefix = target.clone();
        target.extend_from_slice(&self.max_block_number.to_be_bytes());
        (prefix, target)
    }

    fn latest_version_for_key(&self, key: T::Key) -> RocksDbLatestVersionResult<T> {
        let cf = self.cf()?;
        let (prefix, target) = self.exact_lookup_bounds(&key);
        let read_options = exact_prefix_read_options(&prefix);
        let mut iter = self.snapshot.snapshot().raw_iterator_cf_opt(&cf, read_options);
        iter.seek_for_prev(&target);
        if !iter.valid() {
            iter.status().map_err(rocksdb_error)?;
            return Ok(None);
        }

        let raw_key = iter.key().ok_or(DatabaseError::Decode)?;
        if !raw_key.starts_with(&prefix) {
            return Ok(None);
        }

        let (decoded_key, _) = decode_history_key::<T>(raw_key)?;
        let raw_value = iter.value().ok_or(DatabaseError::Decode)?;
        let value = T::Value::decompress(raw_value)?;
        Ok(Some((decoded_key, value)))
    }

    fn latest_value_for_key(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let cf = self.cf()?;
        let (prefix, target) = self.exact_lookup_bounds(&key);
        let read_options = exact_prefix_read_options(&prefix);
        let mut iter = self.snapshot.snapshot().raw_iterator_cf_opt(&cf, read_options);
        iter.seek_for_prev(&target);
        if !iter.valid() {
            iter.status().map_err(rocksdb_error)?;
            return Ok(None);
        }

        let raw_key = iter.key().ok_or(DatabaseError::Decode)?;
        if !raw_key.starts_with(&prefix) {
            return Ok(None);
        }

        let raw_value = iter.value().ok_or(DatabaseError::Decode)?;
        T::Value::decompress(raw_value).map(Some)
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
        self.next_live_from(start_key)
    }

    fn next(&mut self) -> Result<Option<(T::Key, V)>, DatabaseError> {
        if let Some(key) = self.current_key.clone() {
            self.next_live_after(key)
        } else {
            self.next_live_from(T::Key::default())
        }
    }

    fn next_live_from(&mut self, key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        self.next_live_candidate(key, false, total_order_read_options())
    }

    fn next_live_after(&mut self, key: T::Key) -> Result<Option<(T::Key, V)>, DatabaseError> {
        self.next_live_candidate(key, true, total_order_read_options())
    }

    fn seek_with_prefix(
        &mut self,
        key: T::Key,
        prefix: Vec<u8>,
    ) -> Result<Option<(T::Key, V)>, DatabaseError> {
        self.next_live_candidate(key, false, bounded_prefix_read_options(&prefix))
    }

    fn next_with_prefix(&mut self, prefix: Vec<u8>) -> Result<Option<(T::Key, V)>, DatabaseError> {
        if let Some(key) = self.current_key.clone() {
            self.next_live_candidate(key, true, bounded_prefix_read_options(&prefix))
        } else {
            self.next_live_candidate(T::Key::default(), false, bounded_prefix_read_options(&prefix))
        }
    }

    fn next_live_candidate(
        &mut self,
        key: T::Key,
        exclusive: bool,
        read_options: ReadOptions,
    ) -> Result<Option<(T::Key, V)>, DatabaseError> {
        let found = {
            let cf = self.cf()?;
            let start_block = if exclusive { u64::MAX } else { 0 };
            let start_key = encode_history_key::<T>(&key, start_block);
            let iter = self.snapshot.snapshot().iterator_cf_opt(
                &cf,
                read_options,
                IteratorMode::From(&start_key, Direction::Forward),
            );
            let mut last_candidate = None;
            let mut found = None;

            for item in iter {
                let (raw_key, _) = item.map_err(rocksdb_error)?;
                let (candidate, _) = decode_history_key::<T>(&raw_key)?;
                let before_start = if exclusive { candidate <= key } else { candidate < key };
                if before_start || last_candidate.as_ref() == Some(&candidate) {
                    continue;
                }

                last_candidate = Some(candidate.clone());
                if let Some((live_key, latest_value)) = self.latest_version_for_key(candidate)?
                    && let MaybeDeleted(Some(value)) = latest_value.value
                {
                    found = Some((live_key, value));
                    break;
                }
            }

            found
        };

        if let Some((key, value)) = found {
            self.current_key = Some(key.clone());
            return Ok(Some((key, value)));
        }

        self.current_key = None;
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
            .seek_with_prefix(key, hashed_address_prefix(address))?
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
            .next_with_prefix(hashed_address_prefix(address))?
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
        let result = self
            .inner
            .seek_with_prefix(storage_key, hashed_address_prefix(self.hashed_address))?
            .and_then(|(key, value)| {
                (key.hashed_address == self.hashed_address)
                    .then_some((key.hashed_storage_key, value.0))
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
            let result = self
                .inner
                .next_with_prefix(hashed_address_prefix(self.hashed_address))?
                .and_then(|(key, value)| {
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

fn bounded_prefix_read_options(prefix: &[u8]) -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_iterate_lower_bound(prefix.to_vec());
    if let Some(upper_bound) = prefix_upper_bound(prefix) {
        read_options.set_iterate_upper_bound(upper_bound);
    }
    read_options.set_total_order_seek(true);
    read_options
}

fn exact_prefix_read_options(prefix: &[u8]) -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_iterate_lower_bound(prefix.to_vec());
    if let Some(upper_bound) = prefix_upper_bound(prefix) {
        read_options.set_iterate_upper_bound(upper_bound);
    }
    read_options.set_prefix_same_as_start(true);
    read_options
}

fn total_order_read_options() -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_total_order_seek(true);
    read_options
}

fn hashed_address_prefix(hashed_address: B256) -> Vec<u8> {
    hashed_address.as_slice().to_vec()
}

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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use super::*;

    fn temp_storage() -> (Arc<RocksdbProofsStorage>, TempDir) {
        let dir = TempDir::new().unwrap();
        let storage = Arc::new(RocksdbProofsStorage::new(dir.path()).unwrap());
        (storage, dir)
    }

    #[test]
    fn max_total_wal_size_tracks_column_family_write_buffers() {
        let options = RocksdbProofsStorageOptions::default();
        assert_eq!(
            RocksdbProofsStorage::max_total_wal_size(options),
            RocksdbProofsStorage::column_families().len() as u64
                * options.write_buffer_size as u64
                * options.max_write_buffer_number as u64
        );
    }

    #[test]
    fn max_total_wal_size_uses_explicit_override() {
        let options = RocksdbProofsStorageOptions {
            max_total_wal_size: Some(512 * 1024 * 1024),
            ..Default::default()
        };
        assert_eq!(RocksdbProofsStorage::max_total_wal_size(options), 512 * 1024 * 1024);
    }

    #[test]
    fn default_options_use_sync_friendly_settings() {
        let options = RocksdbProofsStorageOptions::default();
        assert_eq!(options.compression, RocksdbProofsCompression::Lz4);
        assert_eq!(options.bottommost_compression, RocksdbProofsCompression::Lz4);
        assert_eq!(options.compression.db_compression_type(), DBCompressionType::Lz4);
        assert_eq!(options.bottommost_compression.db_compression_type(), DBCompressionType::Lz4);
        assert_eq!(options.block_cache_size, DEFAULT_BLOCK_CACHE_SIZE);
        assert_eq!(options.bytes_per_sync, DEFAULT_BYTES_PER_SYNC);
        assert_eq!(options.compaction_readahead_size, DEFAULT_COMPACTION_READAHEAD_SIZE);
        assert_eq!(
            options.level_zero_file_num_compaction_trigger,
            DEFAULT_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER
        );
        assert_eq!(
            options.level_zero_slowdown_writes_trigger,
            DEFAULT_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER
        );
        assert_eq!(options.level_zero_stop_writes_trigger, DEFAULT_LEVEL_ZERO_STOP_WRITES_TRIGGER);
        assert_eq!(options.max_background_jobs, DEFAULT_MAX_BACKGROUND_JOBS);
        assert_eq!(options.max_subcompactions, DEFAULT_MAX_SUBCOMPACTIONS);
        assert_eq!(options.max_write_buffer_number, DEFAULT_MAX_WRITE_BUFFER_NUMBER);
        assert_eq!(options.target_file_size_base, DEFAULT_TARGET_FILE_SIZE_BASE);
        assert_eq!(options.write_buffer_size, DEFAULT_WRITE_BUFFER_SIZE);
        assert!(options.use_direct_io_for_flush_and_compaction);
        assert_eq!(options.rate_limit_bytes_per_sec, None);
    }

    #[test]
    fn history_prefix_lengths_match_encoded_key_prefixes() {
        let account_path = StoredNibbles(Nibbles::from_nibbles_unchecked([1, 2, 3]));
        let storage_path = StorageTrieKey::new(
            B256::repeat_byte(0x01),
            StoredNibbles(Nibbles::from_nibbles_unchecked([4, 5, 6])),
        );
        let hashed_account = B256::repeat_byte(0x02);
        let hashed_storage =
            HashedStorageKey::new(B256::repeat_byte(0x03), B256::repeat_byte(0x04));

        assert_eq!(
            RocksdbProofsStorage::history_prefix_len(<AccountTrieHistory as Table>::NAME),
            Some(PACKED_NIBBLES_KEY_LEN)
        );
        assert_eq!(
            RocksdbProofsStorage::history_prefix_len(<StorageTrieHistory as Table>::NAME),
            Some(HASH_KEY_LEN + PACKED_NIBBLES_KEY_LEN)
        );
        assert_eq!(
            RocksdbProofsStorage::history_prefix_len(<HashedAccountHistory as Table>::NAME),
            Some(HASH_KEY_LEN)
        );
        assert_eq!(
            RocksdbProofsStorage::history_prefix_len(<HashedStorageHistory as Table>::NAME),
            Some(HASH_KEY_LEN * 2)
        );
        assert_eq!(RocksdbProofsStorage::history_prefix_len(<ProofWindow as Table>::NAME), None);

        assert_eq!(
            encode_history_key_prefix::<AccountTrieHistory>(&account_path).len(),
            RocksdbProofsStorage::history_prefix_len(<AccountTrieHistory as Table>::NAME).unwrap()
        );
        assert_eq!(
            encode_history_key_prefix::<StorageTrieHistory>(&storage_path).len(),
            RocksdbProofsStorage::history_prefix_len(<StorageTrieHistory as Table>::NAME).unwrap()
        );
        assert_eq!(
            encode_history_key_prefix::<HashedAccountHistory>(&hashed_account).len(),
            RocksdbProofsStorage::history_prefix_len(<HashedAccountHistory as Table>::NAME)
                .unwrap()
        );
        assert_eq!(
            encode_history_key_prefix::<HashedStorageHistory>(&hashed_storage).len(),
            RocksdbProofsStorage::history_prefix_len(<HashedStorageHistory as Table>::NAME)
                .unwrap()
        );
    }

    #[test]
    fn opens_with_history_prefix_filter_options() {
        let dir = TempDir::new().unwrap();
        let storage = RocksdbProofsStorage::new_with_options(
            dir.path(),
            RocksdbProofsStorageOptions::default(),
        )
        .unwrap();
        let account = B256::repeat_byte(0x55);

        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();
        storage.store_trie_updates(block(1, B256::ZERO), account_update(account, 1)).unwrap();

        assert_eq!(storage.get_latest_block_number().unwrap(), Some((1, B256::repeat_byte(1))));
        assert_eq!(storage.account_by_hashed_key(account, 1).unwrap().unwrap().nonce, 1);
    }

    #[test]
    fn storage_address_prefix_cursors_seek_after_flush_and_compaction() {
        let (storage, _dir) = temp_storage();
        let hashed_address = B256::repeat_byte(0x42);
        let storage_key = B256::repeat_byte(0x10);
        let storage_value = U256::from(100);
        let storage_path = Nibbles::from_nibbles_unchecked([1, 2, 3]);
        let storage_node =
            BranchNodeCompact { root_hash: Some(B256::repeat_byte(0x33)), ..Default::default() };

        storage.store_hashed_storages(hashed_address, vec![(storage_key, storage_value)]).unwrap();
        storage
            .store_storage_branches(
                hashed_address,
                vec![(storage_path.clone(), Some(storage_node.clone()))],
            )
            .unwrap();
        storage.flush_and_compact().unwrap();

        let mut hashed_cursor = storage.storage_hashed_cursor(hashed_address, 0).unwrap();
        assert_eq!(hashed_cursor.seek(B256::ZERO).unwrap(), Some((storage_key, storage_value)));
        assert_eq!(hashed_cursor.next().unwrap(), None);

        let mut trie_cursor = storage.storage_trie_cursor(hashed_address, 0).unwrap();
        assert_eq!(
            trie_cursor.seek(Nibbles::default()).unwrap(),
            Some((storage_path, storage_node))
        );
        assert_eq!(trie_cursor.next().unwrap(), None);
    }

    fn block(number: u64, parent: B256) -> BlockWithParent {
        BlockWithParent {
            parent,
            block: BlockNumHash {
                number,
                hash: if number == 0 { B256::ZERO } else { B256::repeat_byte(number as u8) },
            },
        }
    }

    fn account_update(address: B256, nonce: u64) -> BlockStateDiff {
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(address, Some(Account { nonce, ..Default::default() }));

        BlockStateDiff {
            sorted_trie_updates: TrieUpdates::default().into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        }
    }

    fn assert_completes(rx: mpsc::Receiver<BaseProofsStorageResult<()>>) {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("operation should complete")
            .expect("operation should succeed");
    }

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

    #[test]
    fn append_can_run_while_prune_holds_history_read_gate() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();

        let _prune_guard = storage.prune_lock.lock();
        let _history_guard = storage.history_gate.read();
        let (tx, rx) = mpsc::channel();
        let task_storage = Arc::clone(&storage);

        thread::spawn(move || {
            let result = task_storage
                .store_trie_updates(block(1, B256::ZERO), BlockStateDiff::default())
                .map(|_| ());
            tx.send(result).unwrap();
        });

        assert_completes(rx);
        assert_eq!(storage.get_latest_block_number().unwrap(), Some((1, B256::repeat_byte(1))));
    }

    #[test]
    fn exclusive_history_gate_blocks_append() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();

        let history_guard = storage.history_gate.write();
        let (tx, rx) = mpsc::channel();
        let task_storage = Arc::clone(&storage);

        thread::spawn(move || {
            let result = task_storage
                .store_trie_updates(block(1, B256::ZERO), BlockStateDiff::default())
                .map(|_| ());
            tx.send(result).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(history_guard);
        assert_completes(rx);
    }

    #[test]
    fn append_lock_serializes_append_writers() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();

        let append_guard = storage.append_lock.lock();
        let (tx, rx) = mpsc::channel();
        let task_storage = Arc::clone(&storage);

        thread::spawn(move || {
            let result = task_storage
                .store_trie_updates(block(1, B256::ZERO), BlockStateDiff::default())
                .map(|_| ());
            tx.send(result).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(append_guard);
        assert_completes(rx);
    }

    #[test]
    fn prune_history_read_gate_blocks_history_rewrite() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();

        let _prune_guard = storage.prune_lock.lock();
        let history_guard = storage.history_gate.read();
        let (tx, rx) = mpsc::channel();
        let task_storage = Arc::clone(&storage);

        thread::spawn(move || {
            let result = task_storage.replace_updates(BlockNumHash::new(0, B256::ZERO), vec![]);
            tx.send(result).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(history_guard);
        assert_completes(rx);
    }

    #[test]
    fn append_during_prepared_prune_preserves_latest_block() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();
        storage.store_trie_updates(block(1, B256::ZERO), BlockStateDiff::default()).unwrap();
        storage
            .store_trie_updates(block(2, B256::repeat_byte(1)), BlockStateDiff::default())
            .unwrap();

        let _prune_guard = storage.prune_lock.lock();
        let _history_guard = storage.history_gate.read();
        let prepared =
            storage.prepare_prune(block(1, B256::ZERO)).unwrap().expect("prepared prune");

        storage
            .store_trie_updates(block(3, B256::repeat_byte(2)), BlockStateDiff::default())
            .unwrap();
        storage.commit_prepared_prune(prepared).unwrap();

        assert_eq!(storage.get_earliest_block_number().unwrap(), Some((1, B256::repeat_byte(1))));
        assert_eq!(storage.get_latest_block_number().unwrap(), Some((3, B256::repeat_byte(3))));
        let latest_diff = storage.fetch_trie_updates(3).unwrap();
        assert!(latest_diff.sorted_trie_updates.account_nodes_ref().is_empty());
        assert!(latest_diff.sorted_trie_updates.storage_tries_ref().is_empty());
        assert!(latest_diff.sorted_post_state.accounts.is_empty());
        assert!(latest_diff.sorted_post_state.storages.is_empty());
    }

    #[test]
    fn append_inside_requested_prune_range_survives() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number_hash(0, B256::ZERO).unwrap();

        let address = B256::repeat_byte(0xAA);
        storage.store_trie_updates(block(1, B256::ZERO), account_update(address, 1)).unwrap();
        storage
            .store_trie_updates(block(2, B256::repeat_byte(1)), account_update(address, 2))
            .unwrap();

        let _prune_guard = storage.prune_lock.lock();
        let _history_guard = storage.history_gate.read();
        let prepared = storage
            .prepare_prune(block(10, B256::repeat_byte(2)))
            .unwrap()
            .expect("prepared prune");

        storage
            .store_trie_updates(block(3, B256::repeat_byte(2)), account_update(address, 3))
            .unwrap();
        let counts = storage.commit_prepared_prune(prepared).unwrap();

        assert_eq!(counts.hashed_accounts_written_total, 1);
        assert_eq!(counts.account_trie_updates_written_total, 0);
        assert_eq!(counts.storage_trie_updates_written_total, 0);
        assert_eq!(counts.hashed_storages_written_total, 0);
        assert_eq!(storage.get_earliest_block_number().unwrap(), Some((2, B256::repeat_byte(2))));
        assert_eq!(storage.get_latest_block_number().unwrap(), Some((3, B256::repeat_byte(3))));

        let latest_diff = storage.fetch_trie_updates(3).unwrap();
        assert_eq!(
            &latest_diff.sorted_post_state.accounts[..],
            &[(address, Some(Account { nonce: 3, ..Default::default() }))]
        );

        let mut cursor = storage.account_hashed_cursor(3).unwrap();
        assert_eq!(
            cursor.seek(address).unwrap(),
            Some((address, Account { nonce: 3, ..Default::default() }))
        );
    }
}
