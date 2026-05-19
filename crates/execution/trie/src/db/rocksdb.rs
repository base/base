//! `RocksDB` implementation of proofs storage.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
    ops::{Bound, RangeBounds},
    path::Path,
    sync::Arc,
};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
#[cfg(feature = "metrics")]
use metrics::Label;
use parking_lot::{Mutex, RwLock};
use reth_db::{
    DatabaseError,
    table::{Compress, Decompress},
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
    BlockBasedOptions, BoundColumnFamily, Cache, ColumnFamilyDescriptor, CompactionPri,
    DBCompressionType, DBWithThreadMode, Direction, IteratorMode, MultiThreaded, Options,
    ReadOptions, SnapshotWithThreadMode, WriteBatch, WriteOptions,
};

use super::{BlockNumberHash, ProofWindowKey};
use crate::{
    BaseProofsStorageError,
    BaseProofsStorageError::NoBlocksFound,
    BaseProofsStorageResult, BaseProofsStore, BlockStateDiff,
    api::{BaseProofsInitialStateStore, InitialStateAnchor, InitialStateStatus, WriteCounts},
    db::{ChangeSet, HashedStorageKey, MaybeDeleted, StorageTrieKey, StorageValue},
};

type RocksDb = DBWithThreadMode<MultiThreaded>;

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

const V2_SCHEMA_VERSION: &[u8] = b"rocksdb-proof-store-v2";
const V2_SCHEMA_VERSION_KEY: &[u8] = b"schema-version";

const CF_METADATA: &str = "V2Metadata";
const CF_PROOF_WINDOW: &str = "V2ProofWindow";
const CF_BLOCK_CHANGE_SET: &str = "V2BlockChangeSet";
const CF_ACCOUNT_TRIE: &str = "V2AccountTrie";
const CF_STORAGE_TRIE: &str = "V2StorageTrie";
const CF_HASHED_ACCOUNTS: &str = "V2HashedAccounts";
const CF_HASHED_STORAGE: &str = "V2HashedStorage";
const CF_ACCOUNT_TRIE_HISTORY: &str = "V2AccountTrieHistory";
const CF_STORAGE_TRIE_HISTORY: &str = "V2StorageTrieHistory";
const CF_HASHED_ACCOUNT_HISTORY: &str = "V2HashedAccountHistory";
const CF_HASHED_STORAGE_HISTORY: &str = "V2HashedStorageHistory";

const LEGACY_COLUMN_FAMILIES: &[&str] = &[
    "AccountTrieHistory",
    "StorageTrieHistory",
    "HashedAccountHistory",
    "HashedStorageHistory",
    "ProofWindow",
    "BlockChangeSet",
];

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
    /// Maximum `RocksDB` background jobs.
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

/// Logical key/value domain stored by the RocksDB V2 schema.
pub trait RocksDbDomain {
    /// Logical key type for the domain.
    type Key: Clone + Default + Ord + Eq;
    /// Stored value type for the domain.
    type Value: Clone + Compress + Decompress;

    /// Column family that stores the latest value for each key.
    const CURRENT_CF: &'static str;
    /// Column family that stores before-change historical values.
    const HISTORY_CF: &'static str;
    /// Encoded logical-key length, excluding the history block suffix.
    const KEY_LEN: usize;

    /// Encodes a logical key into RocksDB's lexicographic byte ordering.
    fn encode_key(key: &Self::Key) -> Vec<u8>;
    /// Decodes a logical key from RocksDB's lexicographic byte ordering.
    fn decode_key(raw_key: &[u8]) -> Result<Self::Key, DatabaseError>;
}

/// Account trie branch-node domain.
#[derive(Debug)]
pub struct AccountTrieDomain;
/// Storage trie branch-node domain.
#[derive(Debug)]
pub struct StorageTrieDomain;
/// Hashed account leaf domain.
struct HashedAccountDomain;
/// Hashed storage leaf domain.
struct HashedStorageDomain;

impl RocksDbDomain for AccountTrieDomain {
    type Key = StoredNibbles;
    type Value = BranchNodeCompact;

    const CURRENT_CF: &'static str = CF_ACCOUNT_TRIE;
    const HISTORY_CF: &'static str = CF_ACCOUNT_TRIE_HISTORY;
    const KEY_LEN: usize = PACKED_NIBBLES_KEY_LEN;

    fn encode_key(key: &Self::Key) -> Vec<u8> {
        encode_packed_nibbles(&key.0).to_vec()
    }

    fn decode_key(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        decode_packed_nibbles(raw_key).map(StoredNibbles)
    }
}

impl RocksDbDomain for StorageTrieDomain {
    type Key = StorageTrieKey;
    type Value = BranchNodeCompact;

    const CURRENT_CF: &'static str = CF_STORAGE_TRIE;
    const HISTORY_CF: &'static str = CF_STORAGE_TRIE_HISTORY;
    const KEY_LEN: usize = HASH_KEY_LEN + PACKED_NIBBLES_KEY_LEN;

    fn encode_key(key: &Self::Key) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(&encode_packed_nibbles(&key.path.0));
        encoded
    }

    fn decode_key(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        let hashed_address = B256::from_slice(&raw_key[..HASH_KEY_LEN]);
        let path = StoredNibbles(decode_packed_nibbles(&raw_key[HASH_KEY_LEN..])?);
        Ok(StorageTrieKey::new(hashed_address, path))
    }
}

impl RocksDbDomain for HashedAccountDomain {
    type Key = B256;
    type Value = Account;

    const CURRENT_CF: &'static str = CF_HASHED_ACCOUNTS;
    const HISTORY_CF: &'static str = CF_HASHED_ACCOUNT_HISTORY;
    const KEY_LEN: usize = HASH_KEY_LEN;

    fn encode_key(key: &Self::Key) -> Vec<u8> {
        key.as_slice().to_vec()
    }

    fn decode_key(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        Ok(B256::from_slice(raw_key))
    }
}

impl RocksDbDomain for HashedStorageDomain {
    type Key = HashedStorageKey;
    type Value = StorageValue;

    const CURRENT_CF: &'static str = CF_HASHED_STORAGE;
    const HISTORY_CF: &'static str = CF_HASHED_STORAGE_HISTORY;
    const KEY_LEN: usize = HASH_KEY_LEN * 2;

    fn encode_key(key: &Self::Key) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(Self::KEY_LEN);
        encoded.extend_from_slice(key.hashed_address.as_slice());
        encoded.extend_from_slice(key.hashed_storage_key.as_slice());
        encoded
    }

    fn decode_key(raw_key: &[u8]) -> Result<Self::Key, DatabaseError> {
        if raw_key.len() != Self::KEY_LEN {
            return Err(DatabaseError::Decode);
        }
        Ok(HashedStorageKey::new(
            B256::from_slice(&raw_key[..HASH_KEY_LEN]),
            B256::from_slice(&raw_key[HASH_KEY_LEN..]),
        ))
    }
}

/// `RocksDB` implementation of [`BaseProofsStore`].
pub struct RocksdbProofsStorage {
    db: Arc<RocksDb>,
    write_options: WriteOptions,
    append_lock: Mutex<()>,
    prune_lock: Mutex<()>,
    history_gate: RwLock<()>,
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

struct RocksdbV2Cursor<'db, D: RocksDbDomain> {
    snapshot: Arc<RocksdbReadSnapshot<'db>>,
    max_block_number: u64,
    current_key: Option<D::Key>,
    _domain: PhantomData<D>,
}

/// `RocksDB` implementation of [`TrieCursor`].
pub struct RocksdbTrieCursor<'db, D: RocksDbDomain> {
    inner: RocksdbV2Cursor<'db, D>,
    hashed_address: Option<B256>,
}

/// `RocksDB` implementation of [`HashedCursor`] for storage state.
pub struct RocksdbStorageCursor<'db> {
    inner: RocksdbV2Cursor<'db, HashedStorageDomain>,
    hashed_address: B256,
}

/// `RocksDB` implementation of [`HashedCursor`] for account state.
pub struct RocksdbAccountCursor<'db> {
    inner: RocksdbV2Cursor<'db, HashedAccountDomain>,
}

#[derive(Debug, Clone, Copy)]
struct ProofWindowValue {
    earliest: NumHash,
    latest: NumHash,
}

#[derive(Debug, Default, Clone)]
struct PreparedDeletes {
    account_trie: Vec<Vec<u8>>,
    storage_trie: Vec<Vec<u8>>,
    hashed_account: Vec<Vec<u8>>,
    hashed_storage: Vec<Vec<u8>>,
    block_change_sets: Vec<u64>,
    counts: WriteCounts,
}

#[derive(Debug, Default)]
struct ReplacementOverlay {
    account_trie: BTreeMap<StoredNibbles, Option<BranchNodeCompact>>,
    storage_trie: BTreeMap<StorageTrieKey, Option<BranchNodeCompact>>,
    hashed_account: BTreeMap<B256, Option<Account>>,
    hashed_storage: BTreeMap<HashedStorageKey, Option<StorageValue>>,
}

trait ReplacementDomain: RocksDbDomain {
    fn overlay_ref(overlay: &ReplacementOverlay) -> &BTreeMap<Self::Key, Option<Self::Value>>;
    fn overlay_mut(
        overlay: &mut ReplacementOverlay,
    ) -> &mut BTreeMap<Self::Key, Option<Self::Value>>;
}

impl ReplacementDomain for AccountTrieDomain {
    fn overlay_ref(overlay: &ReplacementOverlay) -> &BTreeMap<Self::Key, Option<Self::Value>> {
        &overlay.account_trie
    }

    fn overlay_mut(
        overlay: &mut ReplacementOverlay,
    ) -> &mut BTreeMap<Self::Key, Option<Self::Value>> {
        &mut overlay.account_trie
    }
}

impl ReplacementDomain for StorageTrieDomain {
    fn overlay_ref(overlay: &ReplacementOverlay) -> &BTreeMap<Self::Key, Option<Self::Value>> {
        &overlay.storage_trie
    }

    fn overlay_mut(
        overlay: &mut ReplacementOverlay,
    ) -> &mut BTreeMap<Self::Key, Option<Self::Value>> {
        &mut overlay.storage_trie
    }
}

impl ReplacementDomain for HashedAccountDomain {
    fn overlay_ref(overlay: &ReplacementOverlay) -> &BTreeMap<Self::Key, Option<Self::Value>> {
        &overlay.hashed_account
    }

    fn overlay_mut(
        overlay: &mut ReplacementOverlay,
    ) -> &mut BTreeMap<Self::Key, Option<Self::Value>> {
        &mut overlay.hashed_account
    }
}

impl ReplacementDomain for HashedStorageDomain {
    fn overlay_ref(overlay: &ReplacementOverlay) -> &BTreeMap<Self::Key, Option<Self::Value>> {
        &overlay.hashed_storage
    }

    fn overlay_mut(
        overlay: &mut ReplacementOverlay,
    ) -> &mut BTreeMap<Self::Key, Option<Self::Value>> {
        &mut overlay.hashed_storage
    }
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

impl<D> fmt::Debug for RocksdbV2Cursor<'_, D>
where
    D: RocksDbDomain,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksdbV2Cursor")
            .field("max_block_number", &self.max_block_number)
            .finish_non_exhaustive()
    }
}

impl<D> fmt::Debug for RocksdbTrieCursor<'_, D>
where
    D: RocksDbDomain,
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

impl RocksdbProofsStorage {
    /// Creates a new [`RocksdbProofsStorage`] instance with the given path.
    pub fn new(path: &Path) -> Result<Self, BaseProofsStorageError> {
        Self::new_with_options(path, RocksdbProofsStorageOptions::default())
    }

    /// Creates a new [`RocksdbProofsStorage`] instance with the given path and options.
    pub fn new_with_options(
        path: &Path,
        storage_options: RocksdbProofsStorageOptions,
    ) -> Result<Self, BaseProofsStorageError> {
        Self::ensure_no_legacy_rocksdb(path)?;

        let block_cache = Cache::new_lru_cache(storage_options.block_cache_size);
        let db_options = Self::db_options(&block_cache, storage_options);
        let descriptors = Self::column_families().into_iter().map(|name| {
            ColumnFamilyDescriptor::new(name, Self::cf_options(name, &block_cache, storage_options))
        });
        let db = RocksDb::open_cf_descriptors(&db_options, path, descriptors)
            .map_err(|e| DatabaseError::Other(format!("failed to open RocksDB database: {e}")))?;

        let mut write_options = WriteOptions::default();
        write_options.set_sync(false);

        let storage = Self {
            db: Arc::new(db),
            write_options,
            append_lock: Mutex::new(()),
            prune_lock: Mutex::new(()),
            history_gate: RwLock::new(()),
        };
        storage.ensure_schema_marker()?;
        Ok(storage)
    }

    fn ensure_no_legacy_rocksdb(path: &Path) -> BaseProofsStorageResult<()> {
        if !path.exists() {
            return Ok(());
        }

        let options = Options::default();
        let column_families = match RocksDb::list_cf(&options, path) {
            Ok(column_families) => column_families,
            Err(_) => return Ok(()),
        };

        let has_legacy = column_families
            .iter()
            .any(|cf| LEGACY_COLUMN_FAMILIES.iter().any(|legacy| cf == legacy));
        if has_legacy {
            return Err(DatabaseError::Other(
                "found a legacy RocksDB proof-history database. RocksDB proof-history now uses \
                 the V2 schema; rebuild proofs history with a fresh --proofs-history.storage-path"
                    .to_owned(),
            )
            .into());
        }

        Ok(())
    }

    fn ensure_schema_marker(&self) -> BaseProofsStorageResult<()> {
        let cf = self.cf(CF_METADATA)?;
        match self.db.get_cf(&cf, V2_SCHEMA_VERSION_KEY).map_err(rocksdb_error)? {
            Some(value) if value.as_slice() == V2_SCHEMA_VERSION => Ok(()),
            Some(_) => {
                Err(DatabaseError::Other("unsupported RocksDB proofs schema version".to_owned())
                    .into())
            }
            None => {
                let mut batch = WriteBatch::default();
                batch.put_cf(&cf, V2_SCHEMA_VERSION_KEY, V2_SCHEMA_VERSION);
                self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
                Ok(())
            }
        }
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

    fn cf_options(
        name: &'static str,
        block_cache: &Cache,
        storage_options: RocksdbProofsStorageOptions,
    ) -> Options {
        let table_options = Self::table_options(block_cache);
        let mut options = Options::default();
        options.set_block_based_table_factory(&table_options);
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
        if name == CF_METADATA || name == CF_PROOF_WINDOW {
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

    const fn column_families() -> [&'static str; 11] {
        [
            CF_METADATA,
            CF_PROOF_WINDOW,
            CF_BLOCK_CHANGE_SET,
            CF_ACCOUNT_TRIE,
            CF_STORAGE_TRIE,
            CF_HASHED_ACCOUNTS,
            CF_HASHED_STORAGE,
            CF_ACCOUNT_TRIE_HISTORY,
            CF_STORAGE_TRIE_HISTORY,
            CF_HASHED_ACCOUNT_HISTORY,
            CF_HASHED_STORAGE_HISTORY,
        ]
    }

    fn cf(&self, name: &'static str) -> BaseProofsStorageResult<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| DatabaseError::Other(format!("missing RocksDB column family {name}")))
            .map_err(Into::into)
    }

    fn put_proof_window(
        &self,
        batch: &mut WriteBatch,
        key: ProofWindowKey,
        block_number: u64,
        hash: B256,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(CF_PROOF_WINDOW)?;
        batch.put_cf(
            &cf,
            encode_proof_window_key(key),
            encode_value(&BlockNumberHash::new(block_number, hash)),
        );
        Ok(())
    }

    fn get_block_number_hash(
        &self,
        key: ProofWindowKey,
    ) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        let cf = self.cf(CF_PROOF_WINDOW)?;
        Ok(self
            .db
            .get_cf(&cf, encode_proof_window_key(key))
            .map_err(rocksdb_error)?
            .map(|value| BlockNumberHash::decompress(&value).map(|v| (v.number(), *v.hash())))
            .transpose()?)
    }

    fn get_latest_block_number_hash(&self) -> BaseProofsStorageResult<Option<(u64, B256)>> {
        self.get_block_number_hash(ProofWindowKey::LatestBlock)
    }

    fn get_append_parent_hash(&self) -> BaseProofsStorageResult<B256> {
        if let Some((_, hash)) = self.get_latest_block_number_hash()? {
            return Ok(hash);
        }

        Ok(self
            .get_block_number_hash(ProofWindowKey::EarliestBlock)?
            .map_or(B256::ZERO, |(_, hash)| hash))
    }

    fn get_proof_window(&self) -> BaseProofsStorageResult<Option<ProofWindowValue>> {
        let Some((earliest_number, earliest_hash)) =
            self.get_block_number_hash(ProofWindowKey::EarliestBlock)?
        else {
            return Ok(None);
        };

        let Some((latest_number, latest_hash)) =
            self.get_block_number_hash(ProofWindowKey::LatestBlock)?
        else {
            return Err(DatabaseError::Other(
                "incomplete RocksDB proof window metadata: missing latest block".to_owned(),
            )
            .into());
        };

        Ok(Some(ProofWindowValue {
            earliest: NumHash::new(earliest_number, earliest_hash),
            latest: NumHash::new(latest_number, latest_hash),
        }))
    }

    fn get_initial_state_anchor(&self) -> BaseProofsStorageResult<Option<BlockNumHash>> {
        Ok(self
            .get_block_number_hash(ProofWindowKey::InitialStateAnchor)?
            .map(|(number, hash)| BlockNumHash { number, hash }))
    }

    fn get_latest_current_key<D: RocksDbDomain>(&self) -> BaseProofsStorageResult<Option<D::Key>> {
        let cf = self.cf(D::CURRENT_CF)?;
        let mut iter = self.db.iterator_cf(&cf, IteratorMode::End);
        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (raw_key, _) = item.map_err(rocksdb_error)?;
        D::decode_key(&raw_key).map(Some).map_err(Into::into)
    }

    fn read_current<D: RocksDbDomain>(
        &self,
        key: &D::Key,
    ) -> BaseProofsStorageResult<Option<D::Value>> {
        let cf = self.cf(D::CURRENT_CF)?;
        self.db
            .get_cf(&cf, D::encode_key(key))
            .map_err(rocksdb_error)?
            .map(|value| D::Value::decompress(&value).map_err(Into::into))
            .transpose()
    }

    fn read_current_from_snapshot<D: RocksDbDomain>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: &D::Key,
    ) -> BaseProofsStorageResult<Option<D::Value>> {
        let cf = self.cf(D::CURRENT_CF)?;
        snapshot
            .get_cf(&cf, D::encode_key(key))
            .map_err(rocksdb_error)?
            .map(|value| D::Value::decompress(&value).map_err(Into::into))
            .transpose()
    }

    fn read_history_exact<D: RocksDbDomain>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: &D::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<Option<Option<D::Value>>> {
        let cf = self.cf(D::HISTORY_CF)?;
        snapshot
            .get_cf(&cf, encode_history_key::<D>(key, block_number))
            .map_err(rocksdb_error)?
            .map(|value| {
                MaybeDeleted::<D::Value>::decompress(&value)
                    .map(|value| value.0)
                    .map_err(Into::into)
            })
            .transpose()
    }

    fn value_at<D: RocksDbDomain>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: &D::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<Option<D::Value>> {
        if let Some(next_block) = block_number.checked_add(1)
            && let Some(value) = self.next_history_value::<D>(snapshot, key, next_block)?
        {
            return Ok(value);
        }

        self.read_current_from_snapshot::<D>(snapshot, key)
    }

    fn next_history_value<D: RocksDbDomain>(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: &D::Key,
        min_block_number: u64,
    ) -> BaseProofsStorageResult<Option<Option<D::Value>>> {
        let cf = self.cf(D::HISTORY_CF)?;
        let prefix = D::encode_key(key);
        let start_key = encode_history_key::<D>(key, min_block_number);
        let mut read_options = ReadOptions::default();
        if let Some(upper_bound) = prefix_upper_bound(&prefix) {
            read_options.set_iterate_upper_bound(upper_bound);
        }
        let mut iter = snapshot.iterator_cf_opt(
            &cf,
            read_options,
            IteratorMode::From(&start_key, Direction::Forward),
        );
        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
        if !raw_key.starts_with(&prefix) {
            return Ok(None);
        }
        Ok(Some(MaybeDeleted::<D::Value>::decompress(&raw_value)?.0))
    }

    fn put_current<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        key: &D::Key,
        value: &D::Value,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(D::CURRENT_CF)?;
        batch.put_cf(&cf, D::encode_key(key), encode_value(value));
        Ok(())
    }

    fn delete_current<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        key: &D::Key,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(D::CURRENT_CF)?;
        batch.delete_cf(&cf, D::encode_key(key));
        Ok(())
    }

    fn put_history<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        key: &D::Key,
        block_number: u64,
        value: Option<D::Value>,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(D::HISTORY_CF)?;
        batch.put_cf(
            &cf,
            encode_history_key::<D>(key, block_number),
            encode_value(&MaybeDeleted(value)),
        );
        Ok(())
    }

    fn delete_history<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        key: &D::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(D::HISTORY_CF)?;
        batch.delete_cf(&cf, encode_history_key::<D>(key, block_number));
        Ok(())
    }

    fn put_change_set(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        change_set: &ChangeSet,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(CF_BLOCK_CHANGE_SET)?;
        batch.put_cf(&cf, encode_block_number(block_number), encode_value(change_set));
        Ok(())
    }

    fn get_change_set_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        block_number: u64,
    ) -> BaseProofsStorageResult<Option<ChangeSet>> {
        let cf = self.cf(CF_BLOCK_CHANGE_SET)?;
        snapshot
            .get_cf(&cf, encode_block_number(block_number))
            .map_err(rocksdb_error)?
            .map(|value| ChangeSet::decompress(&value).map_err(Into::into))
            .transpose()
    }

    fn iter_change_sets_from_snapshot(
        &self,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<Vec<(u64, ChangeSet)>> {
        let cf = self.cf(CF_BLOCK_CHANGE_SET)?;
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

    fn scan_current_prefix<D: RocksDbDomain>(
        &self,
        prefix: &[u8],
    ) -> BaseProofsStorageResult<Vec<(D::Key, D::Value)>> {
        let cf = self.cf(D::CURRENT_CF)?;
        let mut read_options = ReadOptions::default();
        if let Some(upper_bound) = prefix_upper_bound(prefix) {
            read_options.set_iterate_upper_bound(upper_bound);
        }
        let iter = self.db.iterator_cf_opt(
            &cf,
            read_options,
            IteratorMode::From(prefix, Direction::Forward),
        );
        let mut rows = Vec::new();
        for item in iter {
            let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
            if !raw_key.starts_with(prefix) {
                break;
            }
            rows.push((D::decode_key(&raw_key)?, D::Value::decompress(&raw_value)?));
        }
        Ok(rows)
    }

    fn scan_current_prefix_with_overlay<D: ReplacementDomain>(
        &self,
        prefix: &[u8],
        overlay: &ReplacementOverlay,
    ) -> BaseProofsStorageResult<Vec<(D::Key, D::Value)>> {
        let mut rows = self
            .scan_current_prefix::<D>(prefix)?
            .into_iter()
            .map(|(key, value)| (key, Some(value)))
            .collect::<BTreeMap<_, _>>();

        for (key, value) in D::overlay_ref(overlay) {
            if D::encode_key(key).starts_with(prefix) {
                rows.insert(key.clone(), value.clone());
            }
        }

        Ok(rows.into_iter().filter_map(|(key, value)| value.map(|value| (key, value))).collect())
    }

    fn record_update<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        key: D::Key,
        value: Option<D::Value>,
        seen: &mut BTreeSet<D::Key>,
    ) -> BaseProofsStorageResult<bool> {
        if seen.insert(key.clone()) {
            let before = self.read_current::<D>(&key)?;
            self.put_history::<D>(batch, &key, block_number, before)?;
        }

        match value {
            Some(value) => self.put_current::<D>(batch, &key, &value)?,
            None => self.delete_current::<D>(batch, &key)?,
        }

        Ok(true)
    }

    fn record_update_with_overlay<D: ReplacementDomain>(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        key: D::Key,
        value: Option<D::Value>,
        seen: &mut BTreeSet<D::Key>,
        overlay: &mut ReplacementOverlay,
    ) -> BaseProofsStorageResult<bool> {
        if seen.insert(key.clone()) {
            let before = match D::overlay_ref(overlay).get(&key) {
                Some(value) => value.clone(),
                None => self.read_current::<D>(&key)?,
            };
            self.put_history::<D>(batch, &key, block_number, before)?;
        }

        match &value {
            Some(value) => self.put_current::<D>(batch, &key, value)?,
            None => self.delete_current::<D>(batch, &key)?,
        }
        D::overlay_mut(overlay).insert(key, value);

        Ok(true)
    }

    fn store_trie_updates_for_block(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<ChangeSet> {
        self.store_trie_updates_for_block_inner(batch, block_number, block_state_diff, None)
    }

    fn store_trie_updates_for_block_with_overlay(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        block_state_diff: BlockStateDiff,
        overlay: &mut ReplacementOverlay,
    ) -> BaseProofsStorageResult<ChangeSet> {
        self.store_trie_updates_for_block_inner(
            batch,
            block_number,
            block_state_diff,
            Some(overlay),
        )
    }

    fn store_trie_updates_for_block_inner(
        &self,
        batch: &mut WriteBatch,
        block_number: u64,
        block_state_diff: BlockStateDiff,
        mut overlay: Option<&mut ReplacementOverlay>,
    ) -> BaseProofsStorageResult<ChangeSet> {
        let BlockStateDiff { sorted_trie_updates, sorted_post_state } = block_state_diff;

        let mut account_trie_keys = BTreeSet::new();
        let mut storage_trie_keys = BTreeSet::new();
        let mut hashed_account_keys = BTreeSet::new();
        let mut hashed_storage_keys = BTreeSet::new();

        for (path, node) in sorted_trie_updates.account_nodes_ref().iter().cloned() {
            let key = StoredNibbles::from(path);
            if let Some(overlay) = overlay.as_mut() {
                self.record_update_with_overlay::<AccountTrieDomain>(
                    batch,
                    block_number,
                    key,
                    node,
                    &mut account_trie_keys,
                    overlay,
                )?;
            } else {
                self.record_update::<AccountTrieDomain>(
                    batch,
                    block_number,
                    key,
                    node,
                    &mut account_trie_keys,
                )?;
            }
        }

        for (hashed_address, nodes) in sorted_trie_updates.storage_tries_ref() {
            if nodes.is_deleted {
                let prefix = storage_prefix(*hashed_address);
                let wiped = if let Some(overlay) = overlay.as_ref() {
                    self.scan_current_prefix_with_overlay::<StorageTrieDomain>(&prefix, overlay)?
                } else {
                    self.scan_current_prefix::<StorageTrieDomain>(&prefix)?
                };
                for (key, _) in wiped {
                    if let Some(overlay) = overlay.as_mut() {
                        self.record_update_with_overlay::<StorageTrieDomain>(
                            batch,
                            block_number,
                            key,
                            None,
                            &mut storage_trie_keys,
                            overlay,
                        )?;
                    } else {
                        self.record_update::<StorageTrieDomain>(
                            batch,
                            block_number,
                            key,
                            None,
                            &mut storage_trie_keys,
                        )?;
                    }
                }
            }

            for (path, node) in nodes.storage_nodes_ref().iter().cloned() {
                let key = StorageTrieKey::new(*hashed_address, StoredNibbles::from(path));
                if let Some(overlay) = overlay.as_mut() {
                    self.record_update_with_overlay::<StorageTrieDomain>(
                        batch,
                        block_number,
                        key,
                        node,
                        &mut storage_trie_keys,
                        overlay,
                    )?;
                } else {
                    self.record_update::<StorageTrieDomain>(
                        batch,
                        block_number,
                        key,
                        node,
                        &mut storage_trie_keys,
                    )?;
                }
            }
        }

        for (hashed_address, account) in sorted_post_state.accounts.iter().copied() {
            if let Some(overlay) = overlay.as_mut() {
                self.record_update_with_overlay::<HashedAccountDomain>(
                    batch,
                    block_number,
                    hashed_address,
                    account,
                    &mut hashed_account_keys,
                    overlay,
                )?;
            } else {
                self.record_update::<HashedAccountDomain>(
                    batch,
                    block_number,
                    hashed_address,
                    account,
                    &mut hashed_account_keys,
                )?;
            }
        }

        for (hashed_address, storage) in sorted_post_state.storages {
            if storage.is_wiped() {
                let prefix = storage_prefix(hashed_address);
                let wiped = if let Some(overlay) = overlay.as_ref() {
                    self.scan_current_prefix_with_overlay::<HashedStorageDomain>(&prefix, overlay)?
                } else {
                    self.scan_current_prefix::<HashedStorageDomain>(&prefix)?
                };
                for (key, _) in wiped {
                    if let Some(overlay) = overlay.as_mut() {
                        self.record_update_with_overlay::<HashedStorageDomain>(
                            batch,
                            block_number,
                            key,
                            None,
                            &mut hashed_storage_keys,
                            overlay,
                        )?;
                    } else {
                        self.record_update::<HashedStorageDomain>(
                            batch,
                            block_number,
                            key,
                            None,
                            &mut hashed_storage_keys,
                        )?;
                    }
                }
            }

            for (hashed_storage_key, value) in storage.storage_slots_ref() {
                let key = HashedStorageKey::new(hashed_address, *hashed_storage_key);
                let value = (!value.is_zero()).then_some(StorageValue(*value));
                if let Some(overlay) = overlay.as_mut() {
                    self.record_update_with_overlay::<HashedStorageDomain>(
                        batch,
                        block_number,
                        key,
                        value,
                        &mut hashed_storage_keys,
                        overlay,
                    )?;
                } else {
                    self.record_update::<HashedStorageDomain>(
                        batch,
                        block_number,
                        key,
                        value,
                        &mut hashed_storage_keys,
                    )?;
                }
            }
        }

        Ok(ChangeSet {
            account_trie_keys: account_trie_keys.into_iter().collect(),
            storage_trie_keys: storage_trie_keys.into_iter().collect(),
            hashed_account_keys: hashed_account_keys.into_iter().collect(),
            hashed_storage_keys: hashed_storage_keys.into_iter().collect(),
        })
    }

    fn store_trie_updates_append_only(
        &self,
        batch: &mut WriteBatch,
        block_ref: BlockWithParent,
        block_state_diff: BlockStateDiff,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let block_number = block_ref.block.number;
        let latest_block_hash = self.get_append_parent_hash()?;

        if latest_block_hash != block_ref.parent {
            return Err(BaseProofsStorageError::OutOfOrder {
                block_number,
                parent_block_hash: block_ref.parent,
                latest_block_hash,
            });
        }

        let change_set =
            self.store_trie_updates_for_block(batch, block_number, block_state_diff)?;
        self.put_change_set(batch, block_number, &change_set)?;
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

    fn prepare_history_deletes(
        &self,
        block_range: impl RangeBounds<u64>,
    ) -> BaseProofsStorageResult<PreparedDeletes> {
        let snapshot = self.db.snapshot();
        let mut deletes = PreparedDeletes::default();
        let mut account_trie_keys = BTreeSet::new();
        let mut storage_trie_keys = BTreeSet::new();
        let mut hashed_account_keys = BTreeSet::new();
        let mut hashed_storage_keys = BTreeSet::new();

        for (block_number, change_set) in
            self.iter_change_sets_from_snapshot(&snapshot, block_range)?
        {
            deletes.block_change_sets.push(block_number);
            account_trie_keys.extend(change_set.account_trie_keys.iter().cloned());
            storage_trie_keys.extend(change_set.storage_trie_keys.iter().cloned());
            hashed_account_keys.extend(change_set.hashed_account_keys.iter().cloned());
            hashed_storage_keys.extend(change_set.hashed_storage_keys.iter().cloned());
            deletes.account_trie.extend(
                change_set
                    .account_trie_keys
                    .iter()
                    .map(|key| encode_history_key::<AccountTrieDomain>(key, block_number)),
            );
            deletes.storage_trie.extend(
                change_set
                    .storage_trie_keys
                    .iter()
                    .map(|key| encode_history_key::<StorageTrieDomain>(key, block_number)),
            );
            deletes.hashed_account.extend(
                change_set
                    .hashed_account_keys
                    .iter()
                    .map(|key| encode_history_key::<HashedAccountDomain>(key, block_number)),
            );
            deletes.hashed_storage.extend(
                change_set
                    .hashed_storage_keys
                    .iter()
                    .map(|key| encode_history_key::<HashedStorageDomain>(key, block_number)),
            );
        }

        deletes.counts = WriteCounts {
            account_trie_updates_written_total: account_trie_keys.len() as u64,
            storage_trie_updates_written_total: storage_trie_keys.len() as u64,
            hashed_accounts_written_total: hashed_account_keys.len() as u64,
            hashed_storages_written_total: hashed_storage_keys.len() as u64,
        };

        Ok(deletes)
    }

    fn apply_history_deletes(
        &self,
        batch: &mut WriteBatch,
        deletes: PreparedDeletes,
    ) -> BaseProofsStorageResult<WriteCounts> {
        let counts = deletes.counts.clone();

        self.delete_raw_history_keys::<AccountTrieDomain>(batch, deletes.account_trie)?;
        self.delete_raw_history_keys::<StorageTrieDomain>(batch, deletes.storage_trie)?;
        self.delete_raw_history_keys::<HashedAccountDomain>(batch, deletes.hashed_account)?;
        self.delete_raw_history_keys::<HashedStorageDomain>(batch, deletes.hashed_storage)?;

        let cf = self.cf(CF_BLOCK_CHANGE_SET)?;
        for block_number in deletes.block_change_sets {
            batch.delete_cf(&cf, encode_block_number(block_number));
        }

        Ok(counts)
    }

    fn delete_raw_history_keys<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        keys: Vec<Vec<u8>>,
    ) -> BaseProofsStorageResult<()> {
        let cf = self.cf(D::HISTORY_CF)?;
        for key in keys {
            batch.delete_cf(&cf, key);
        }
        Ok(())
    }

    fn restore_domain_value<D: RocksDbDomain>(
        &self,
        batch: &mut WriteBatch,
        snapshot: &SnapshotWithThreadMode<'_, RocksDb>,
        key: &D::Key,
        block_number: u64,
    ) -> BaseProofsStorageResult<Option<D::Value>> {
        let before =
            self.read_history_exact::<D>(snapshot, key, block_number)?.ok_or_else(|| {
                DatabaseError::Other(format!(
                    "missing RocksDB V2 history row for unwind at block {block_number}"
                ))
            })?;

        match &before {
            Some(value) => self.put_current::<D>(batch, key, value)?,
            None => self.delete_current::<D>(batch, key)?,
        }
        self.delete_history::<D>(batch, key, block_number)?;
        Ok(before)
    }

    fn restore_blocks_descending(
        &self,
        batch: &mut WriteBatch,
        from_block: u64,
        to_block_inclusive: u64,
    ) -> BaseProofsStorageResult<()> {
        self.restore_blocks_descending_inner(batch, from_block, to_block_inclusive, None)
    }

    fn restore_blocks_descending_with_overlay(
        &self,
        batch: &mut WriteBatch,
        from_block: u64,
        to_block_inclusive: u64,
        overlay: &mut ReplacementOverlay,
    ) -> BaseProofsStorageResult<()> {
        self.restore_blocks_descending_inner(batch, from_block, to_block_inclusive, Some(overlay))
    }

    fn restore_blocks_descending_inner(
        &self,
        batch: &mut WriteBatch,
        from_block: u64,
        to_block_inclusive: u64,
        mut overlay: Option<&mut ReplacementOverlay>,
    ) -> BaseProofsStorageResult<()> {
        if from_block > to_block_inclusive {
            return Ok(());
        }

        let snapshot = self.db.snapshot();
        let mut change_sets =
            self.iter_change_sets_from_snapshot(&snapshot, from_block..=to_block_inclusive)?;
        change_sets.sort_unstable_by(|a, b| b.0.cmp(&a.0));

        let change_set_cf = self.cf(CF_BLOCK_CHANGE_SET)?;
        for (block_number, change_set) in change_sets {
            for key in &change_set.account_trie_keys {
                let before = self.restore_domain_value::<AccountTrieDomain>(
                    batch,
                    &snapshot,
                    key,
                    block_number,
                )?;
                if let Some(overlay) = overlay.as_mut() {
                    overlay.account_trie.insert(key.clone(), before);
                }
            }
            for key in &change_set.storage_trie_keys {
                let before = self.restore_domain_value::<StorageTrieDomain>(
                    batch,
                    &snapshot,
                    key,
                    block_number,
                )?;
                if let Some(overlay) = overlay.as_mut() {
                    overlay.storage_trie.insert(key.clone(), before);
                }
            }
            for key in &change_set.hashed_account_keys {
                let before = self.restore_domain_value::<HashedAccountDomain>(
                    batch,
                    &snapshot,
                    key,
                    block_number,
                )?;
                if let Some(overlay) = overlay.as_mut() {
                    overlay.hashed_account.insert(*key, before);
                }
            }
            for key in &change_set.hashed_storage_keys {
                let before = self.restore_domain_value::<HashedStorageDomain>(
                    batch,
                    &snapshot,
                    key,
                    block_number,
                )?;
                if let Some(overlay) = overlay.as_mut() {
                    overlay.hashed_storage.insert(key.clone(), before);
                }
            }
            batch.delete_cf(&change_set_cf, encode_block_number(block_number));
        }

        Ok(())
    }
}

impl BaseProofsStore for RocksdbProofsStorage {
    type StorageTrieCursor<'tx>
        = RocksdbTrieCursor<'tx, StorageTrieDomain>
    where
        Self: 'tx;
    type AccountTrieCursor<'tx>
        = RocksdbTrieCursor<'tx, AccountTrieDomain>
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
        Ok(RocksdbTrieCursor::<StorageTrieDomain>::new(
            self.db.as_ref(),
            max_block_number,
            Some(hashed_address),
        ))
    }

    fn account_trie_cursor<'tx>(
        &'tx self,
        max_block_number: u64,
    ) -> BaseProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        Ok(RocksdbTrieCursor::<AccountTrieDomain>::new(self.db.as_ref(), max_block_number, None))
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
        Ok(RocksdbTrieCursor::<StorageTrieDomain>::new_with_snapshot(
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
        Ok(RocksdbTrieCursor::<AccountTrieDomain>::new_with_snapshot(
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
            .get_change_set_from_snapshot(&snapshot, block_number)?
            .ok_or(BaseProofsStorageError::NoChangeSetForBlock(block_number))?;

        let mut trie_updates = TrieUpdates::default();
        for key in change_set.account_trie_keys {
            match self.value_at::<AccountTrieDomain>(&snapshot, &key, block_number)? {
                Some(value) => {
                    trie_updates.account_nodes.insert(key.0, value);
                }
                None => {
                    trie_updates.removed_nodes.insert(key.0);
                }
            }
        }

        for key in change_set.storage_trie_keys {
            let storage_updates = trie_updates
                .storage_tries
                .entry(key.hashed_address)
                .or_insert_with(StorageTrieUpdates::default);
            match self.value_at::<StorageTrieDomain>(&snapshot, &key, block_number)? {
                Some(value) => {
                    storage_updates.storage_nodes.insert(key.path.0, value);
                }
                None => {
                    storage_updates.removed_nodes.insert(key.path.0);
                }
            }
        }

        let mut post_state = HashedPostState::with_capacity(change_set.hashed_account_keys.len());
        for key in change_set.hashed_account_keys {
            let value = self.value_at::<HashedAccountDomain>(&snapshot, &key, block_number)?;
            post_state.accounts.insert(key, value);
        }

        for key in change_set.hashed_storage_keys {
            let value = self
                .value_at::<HashedStorageDomain>(&snapshot, &key, block_number)?
                .map_or(U256::ZERO, |value| value.0);
            post_state
                .storages
                .entry(key.hashed_address)
                .or_default()
                .storage
                .insert(key.hashed_storage_key, value);
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
        let _prune_guard = self.prune_lock.lock();
        let _history_guard = self.history_gate.read();

        let Some((earliest, _)) = self.get_block_number_hash(ProofWindowKey::EarliestBlock)? else {
            return Ok(WriteCounts::default());
        };
        let Some((latest, latest_hash)) = self.get_latest_block_number_hash()? else {
            return Ok(WriteCounts::default());
        };

        let (target_block, target_hash) = if new_earliest_block_ref.block.number > latest {
            (latest, latest_hash)
        } else {
            (new_earliest_block_ref.block.number, new_earliest_block_ref.block.hash)
        };

        if earliest >= target_block {
            return Ok(WriteCounts::default());
        }

        let deletes = self.prepare_history_deletes((earliest + 1)..=target_block)?;
        let mut batch = WriteBatch::default();
        let counts = self.apply_history_deletes(&mut batch, deletes)?;
        self.put_proof_window(
            &mut batch,
            ProofWindowKey::EarliestBlock,
            target_block,
            target_hash,
        )?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
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

        let mut batch = WriteBatch::default();
        self.restore_blocks_descending(&mut batch, to.block.number, proof_window.latest.number)?;
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

        let _append_guard = self.append_lock.lock();
        let _guard = self.history_gate.write();
        let current_latest = self.get_latest_block_number_hash()?.map(|(number, _)| number);
        let mut batch = WriteBatch::default();
        let mut overlay = ReplacementOverlay::default();
        if let Some(current_latest) = current_latest
            && let Some(first_removed) = latest_common_block.number.checked_add(1)
            && first_removed <= current_latest
        {
            self.restore_blocks_descending_with_overlay(
                &mut batch,
                first_removed,
                current_latest,
                &mut overlay,
            )?;
        }

        self.put_proof_window(
            &mut batch,
            ProofWindowKey::LatestBlock,
            latest_common_block.number,
            latest_common_block.hash,
        )?;

        for (block_with_parent, diff) in blocks_to_add {
            let block_number = block_with_parent.block.number;
            let change_set = self.store_trie_updates_for_block_with_overlay(
                &mut batch,
                block_number,
                diff,
                &mut overlay,
            )?;
            self.put_change_set(&mut batch, block_number, &change_set)?;
            self.put_proof_window(
                &mut batch,
                ProofWindowKey::LatestBlock,
                block_number,
                block_with_parent.block.hash,
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
        let _guard = self.history_gate.write();
        let mut batch = WriteBatch::default();
        self.put_proof_window(&mut batch, ProofWindowKey::EarliestBlock, block_number, hash)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
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
            latest_account_trie_key: self.get_latest_current_key::<AccountTrieDomain>()?,
            latest_storage_trie_key: self.get_latest_current_key::<StorageTrieDomain>()?,
            latest_hashed_account_key: self.get_latest_current_key::<HashedAccountDomain>()?,
            latest_hashed_storage_key: self.get_latest_current_key::<HashedStorageDomain>()?,
        })
    }

    fn set_initial_state_anchor(&self, anchor: BlockNumHash) -> BaseProofsStorageResult<()> {
        let _guard = self.history_gate.write();
        if self.get_initial_state_anchor()?.is_some() {
            return Err(DatabaseError::Other("initial state anchor already set".to_owned()).into());
        }

        let mut batch = WriteBatch::default();
        self.put_proof_window(
            &mut batch,
            ProofWindowKey::InitialStateAnchor,
            anchor.number,
            anchor.hash,
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
        for (path, node) in account_nodes {
            let key = StoredNibbles::from(path);
            match node {
                Some(node) => self.put_current::<AccountTrieDomain>(&mut batch, &key, &node)?,
                None => self.delete_current::<AccountTrieDomain>(&mut batch, &key)?,
            }
        }
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
        for (path, node) in storage_nodes {
            let key = StorageTrieKey::new(hashed_address, StoredNibbles::from(path));
            match node {
                Some(node) => self.put_current::<StorageTrieDomain>(&mut batch, &key, &node)?,
                None => self.delete_current::<StorageTrieDomain>(&mut batch, &key)?,
            }
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
        for (key, account) in accounts {
            match account {
                Some(account) => {
                    self.put_current::<HashedAccountDomain>(&mut batch, &key, &account)?
                }
                None => self.delete_current::<HashedAccountDomain>(&mut batch, &key)?,
            }
        }
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
        for (hashed_storage_key, value) in storages {
            let key = HashedStorageKey::new(hashed_address, hashed_storage_key);
            if value.is_zero() {
                self.delete_current::<HashedStorageDomain>(&mut batch, &key)?;
            } else {
                self.put_current::<HashedStorageDomain>(&mut batch, &key, &StorageValue(value))?;
            }
        }
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
        Ok(())
    }

    fn commit_initial_state(&self) -> BaseProofsStorageResult<BlockNumHash> {
        let _guard = self.history_gate.write();
        let anchor = self.get_initial_state_anchor()?.ok_or(NoBlocksFound)?;
        let mut batch = WriteBatch::default();
        self.put_proof_window(
            &mut batch,
            ProofWindowKey::EarliestBlock,
            anchor.number,
            anchor.hash,
        )?;
        self.put_proof_window(&mut batch, ProofWindowKey::LatestBlock, anchor.number, anchor.hash)?;
        self.db.write_opt(batch, &self.write_options).map_err(rocksdb_error)?;
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

impl<'db, D> RocksdbV2Cursor<'db, D>
where
    D: RocksDbDomain,
{
    fn new(db: &'db RocksDb, max_block_number: u64) -> Self {
        let snapshot = Arc::new(RocksdbReadSnapshot::new(db));
        Self::new_with_snapshot(snapshot, max_block_number)
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
    ) -> Self {
        Self { snapshot, max_block_number, current_key: None, _domain: PhantomData }
    }

    fn cf(&self, name: &'static str) -> Result<Arc<BoundColumnFamily<'_>>, DatabaseError> {
        self.snapshot.cf(name)
    }

    fn read_current(&self, key: &D::Key) -> Result<Option<D::Value>, DatabaseError> {
        let cf = self.cf(D::CURRENT_CF)?;
        self.snapshot
            .snapshot()
            .get_cf(&cf, D::encode_key(key))
            .map_err(rocksdb_error)?
            .map(|value| D::Value::decompress(&value))
            .transpose()
    }

    fn next_history_value(
        &self,
        key: &D::Key,
        min_block_number: u64,
    ) -> Result<Option<Option<D::Value>>, DatabaseError> {
        let cf = self.cf(D::HISTORY_CF)?;
        let prefix = D::encode_key(key);
        let start_key = encode_history_key::<D>(key, min_block_number);
        let mut read_options = ReadOptions::default();
        if let Some(upper_bound) = prefix_upper_bound(&prefix) {
            read_options.set_iterate_upper_bound(upper_bound);
        }
        let mut iter = self.snapshot.snapshot().iterator_cf_opt(
            &cf,
            read_options,
            IteratorMode::From(&start_key, Direction::Forward),
        );
        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (raw_key, raw_value) = item.map_err(rocksdb_error)?;
        if !raw_key.starts_with(&prefix) {
            return Ok(None);
        }
        Ok(Some(MaybeDeleted::<D::Value>::decompress(&raw_value)?.0))
    }

    fn value_at(&self, key: &D::Key) -> Result<Option<D::Value>, DatabaseError> {
        if let Some(next_block) = self.max_block_number.checked_add(1)
            && let Some(value) = self.next_history_value(key, next_block)?
        {
            return Ok(value);
        }
        self.read_current(key)
    }

    fn seek_exact(&mut self, key: D::Key) -> Result<Option<(D::Key, D::Value)>, DatabaseError> {
        self.current_key = Some(key.clone());
        Ok(self.value_at(&key)?.map(|value| (key, value)))
    }

    fn seek(&mut self, start_key: D::Key) -> Result<Option<(D::Key, D::Value)>, DatabaseError> {
        self.next_live_candidate(start_key, false)
    }

    fn next(&mut self) -> Result<Option<(D::Key, D::Value)>, DatabaseError> {
        if let Some(key) = self.current_key.clone() {
            self.next_live_candidate(key, true)
        } else {
            self.next_live_candidate(D::Key::default(), false)
        }
    }

    fn next_live_candidate(
        &mut self,
        mut key: D::Key,
        mut exclusive: bool,
    ) -> Result<Option<(D::Key, D::Value)>, DatabaseError> {
        loop {
            let current_key = self.next_current_key(&key, exclusive)?;
            let history_key = self.next_history_key(&key, exclusive)?;
            let Some(candidate) = min_option_key(current_key, history_key) else {
                self.current_key = None;
                return Ok(None);
            };

            if let Some(value) = self.value_at(&candidate)? {
                self.current_key = Some(candidate.clone());
                return Ok(Some((candidate, value)));
            }

            key = candidate;
            exclusive = true;
        }
    }

    fn next_current_key(
        &self,
        key: &D::Key,
        exclusive: bool,
    ) -> Result<Option<D::Key>, DatabaseError> {
        let cf = self.cf(D::CURRENT_CF)?;
        let start_key = D::encode_key(key);
        let iter = self
            .snapshot
            .snapshot()
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));
        for item in iter {
            let (raw_key, _) = item.map_err(rocksdb_error)?;
            let candidate = D::decode_key(&raw_key)?;
            if exclusive && candidate <= *key {
                continue;
            }
            return Ok(Some(candidate));
        }
        Ok(None)
    }

    fn next_history_key(
        &self,
        key: &D::Key,
        exclusive: bool,
    ) -> Result<Option<D::Key>, DatabaseError> {
        let cf = self.cf(D::HISTORY_CF)?;
        let start_block = if exclusive { u64::MAX } else { 0 };
        let start_key = encode_history_key::<D>(key, start_block);
        let iter = self
            .snapshot
            .snapshot()
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));
        for item in iter {
            let (raw_key, _) = item.map_err(rocksdb_error)?;
            let (candidate, _) = decode_history_key::<D>(&raw_key)?;
            if exclusive && candidate <= *key {
                continue;
            }
            return Ok(Some(candidate));
        }
        Ok(None)
    }

    const fn is_positioned(&self) -> bool {
        self.current_key.is_some()
    }
}

impl<'db> RocksdbTrieCursor<'db, AccountTrieDomain> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: Option<B256>) -> Self {
        Self { inner: RocksdbV2Cursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: Option<B256>,
    ) -> Self {
        Self {
            inner: RocksdbV2Cursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }
}

impl<'db> RocksdbTrieCursor<'db, StorageTrieDomain> {
    /// Creates a `RocksDB` trie cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: Option<B256>) -> Self {
        Self { inner: RocksdbV2Cursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: Option<B256>,
    ) -> Self {
        Self {
            inner: RocksdbV2Cursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }
}

impl TrieCursor for RocksdbTrieCursor<'_, AccountTrieDomain> {
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

impl TrieCursor for RocksdbTrieCursor<'_, StorageTrieDomain> {
    fn seek_exact(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        let key = StorageTrieKey::new(address, StoredNibbles(path));
        Ok(self.inner.seek_exact(key)?.and_then(|(key, node)| {
            if key.hashed_address == address {
                Some((key.path.0, node))
            } else {
                self.inner.current_key = None;
                None
            }
        }))
    }

    fn seek(
        &mut self,
        path: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        let key = StorageTrieKey::new(address, StoredNibbles(path));
        Ok(self.inner.seek(key)?.and_then(|(key, node)| {
            if key.hashed_address == address {
                Some((key.path.0, node))
            } else {
                self.inner.current_key = None;
                None
            }
        }))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        let Some(address) = self.hashed_address else {
            return Ok(None);
        };
        if !self.inner.is_positioned() {
            return self.seek(Nibbles::default());
        }
        Ok(self.inner.next()?.and_then(|(key, node)| {
            if key.hashed_address == address {
                Some((key.path.0, node))
            } else {
                self.inner.current_key = None;
                None
            }
        }))
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

impl TrieStorageCursor for RocksdbTrieCursor<'_, StorageTrieDomain> {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = Some(hashed_address);
        self.inner.current_key = None;
    }
}

impl<'db> RocksdbStorageCursor<'db> {
    /// Creates a `RocksDB` storage cursor.
    pub fn new(db: &'db RocksDb, max_block_number: u64, hashed_address: B256) -> Self {
        Self { inner: RocksdbV2Cursor::new(db, max_block_number), hashed_address }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
        hashed_address: B256,
    ) -> Self {
        Self {
            inner: RocksdbV2Cursor::new_with_snapshot(snapshot, max_block_number),
            hashed_address,
        }
    }

    fn next_matching_storage(
        &mut self,
        mut candidate: Option<(HashedStorageKey, StorageValue)>,
    ) -> Result<Option<(B256, U256)>, DatabaseError> {
        loop {
            let Some((key, value)) = candidate else {
                return Ok(None);
            };

            if key.hashed_address != self.hashed_address {
                self.inner.current_key = None;
                return Ok(None);
            }

            if !value.0.is_zero() {
                return Ok(Some((key.hashed_storage_key, value.0)));
            }

            candidate = self.inner.next()?;
        }
    }
}

impl HashedCursor for RocksdbStorageCursor<'_> {
    type Value = U256;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        let storage_key = HashedStorageKey::new(self.hashed_address, key);
        let candidate = self.inner.seek(storage_key)?;
        self.next_matching_storage(candidate)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.inner.is_positioned() {
            return self.seek(B256::ZERO);
        }

        let candidate = self.inner.next()?;
        self.next_matching_storage(candidate)
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
        Self { inner: RocksdbV2Cursor::new(db, max_block_number) }
    }

    const fn new_with_snapshot(
        snapshot: Arc<RocksdbReadSnapshot<'db>>,
        max_block_number: u64,
    ) -> Self {
        Self { inner: RocksdbV2Cursor::new_with_snapshot(snapshot, max_block_number) }
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

fn encode_value<T: Compress>(value: &T) -> Vec<u8> {
    let mut encoded = <T as Compress>::Compressed::default();
    value.compress_to_buf(&mut encoded);
    encoded.into()
}

fn encode_proof_window_key(key: ProofWindowKey) -> [u8; 1] {
    [key as u8]
}

fn encode_history_key<D: RocksDbDomain>(key: &D::Key, block_number: u64) -> Vec<u8> {
    let mut encoded = D::encode_key(key);
    encoded.extend_from_slice(&block_number.to_be_bytes());
    encoded
}

fn decode_history_key<D: RocksDbDomain>(raw_key: &[u8]) -> Result<(D::Key, u64), DatabaseError> {
    if raw_key.len() != D::KEY_LEN + BLOCK_NUMBER_KEY_LEN {
        return Err(DatabaseError::Decode);
    }
    let split = D::KEY_LEN;
    let key = D::decode_key(&raw_key[..split])?;
    let block_number =
        u64::from_be_bytes(raw_key[split..].try_into().map_err(|_| DatabaseError::Decode)?);
    Ok((key, block_number))
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

fn storage_prefix(hashed_address: B256) -> [u8; HASH_KEY_LEN] {
    hashed_address.0
}

fn min_option_key<K: Ord>(left: Option<K>, right: Option<K>) -> Option<K> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use reth_trie::HashedStorage;
    use tempfile::TempDir;

    use super::*;

    fn temp_storage() -> (RocksdbProofsStorage, TempDir) {
        let dir = TempDir::new().unwrap();
        let storage = RocksdbProofsStorage::new(dir.path()).unwrap();
        (storage, dir)
    }

    fn block(parent: B256, number: u64, hash_byte: u8) -> BlockWithParent {
        BlockWithParent::new(parent, NumHash::new(number, B256::repeat_byte(hash_byte)))
    }

    fn branch(hash_byte: u8) -> BranchNodeCompact {
        BranchNodeCompact::new(0b1, 0, 0, vec![], Some(B256::repeat_byte(hash_byte)))
    }

    fn account(nonce: u64) -> Account {
        Account {
            nonce,
            balance: U256::from(nonce * 100),
            bytecode_hash: Some(B256::repeat_byte(nonce as u8)),
        }
    }

    fn full_diff(
        account_path: Nibbles,
        account_node: Option<BranchNodeCompact>,
        hashed_address: B256,
        storage_path: Nibbles,
        storage_node: Option<BranchNodeCompact>,
        hashed_account: Option<Account>,
        hashed_storage_key: B256,
        hashed_storage_value: U256,
    ) -> BlockStateDiff {
        let mut trie_updates = TrieUpdates::default();
        match account_node {
            Some(account_node) => {
                trie_updates.account_nodes.insert(account_path, account_node);
            }
            None => {
                trie_updates.removed_nodes.insert(account_path);
            }
        }

        let storage_updates = trie_updates.storage_tries.entry(hashed_address).or_default();
        match storage_node {
            Some(storage_node) => {
                storage_updates.storage_nodes.insert(storage_path, storage_node);
            }
            None => {
                storage_updates.removed_nodes.insert(storage_path);
            }
        }

        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(hashed_address, hashed_account);
        post_state
            .storages
            .entry(hashed_address)
            .or_default()
            .storage
            .insert(hashed_storage_key, hashed_storage_value);

        BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        }
    }

    fn account_diff(hashed_address: B256, account: Option<Account>) -> BlockStateDiff {
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(hashed_address, account);

        BlockStateDiff {
            sorted_trie_updates: TrieUpdates::default().into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        }
    }

    fn storage_diff(hashed_address: B256, hashed_storage_key: B256, value: U256) -> BlockStateDiff {
        let mut post_state = HashedPostState::default();
        post_state
            .storages
            .entry(hashed_address)
            .or_default()
            .storage
            .insert(hashed_storage_key, value);

        BlockStateDiff {
            sorted_trie_updates: TrieUpdates::default().into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        }
    }

    fn wiped_storage_diff(hashed_address: B256) -> BlockStateDiff {
        let mut post_state = HashedPostState::default();
        post_state.storages.insert(hashed_address, HashedStorage::new(true));

        BlockStateDiff {
            sorted_trie_updates: TrieUpdates::default().into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        }
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
            vec![0, 0, 15],
            vec![0, 1],
            vec![0, 15],
            vec![1],
            vec![1, 0],
            vec![1, 1],
            vec![1, 15],
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
    fn packed_nibbles_preserve_exhaustive_short_lexicographic_order() {
        let mut keys = Vec::new();
        for len in 0..=3 {
            let total = 16usize.pow(len);
            for mut value in 0..total {
                let mut nibbles = vec![0; len as usize];
                for nibble in nibbles.iter_mut().rev() {
                    *nibble = (value & 0x0f) as u8;
                    value >>= 4;
                }
                keys.push(Nibbles::from_nibbles_unchecked(nibbles));
            }
        }

        let mut logical = keys.clone();
        logical.sort();
        let mut encoded = keys;
        encoded.sort_by_key(encode_packed_nibbles);

        assert_eq!(encoded, logical);
    }

    #[test]
    fn storage_trie_domain_key_order_matches_logical_address_then_path_order() {
        let addr_1 = B256::repeat_byte(0x10);
        let addr_2 = B256::repeat_byte(0x20);
        let keys = [
            StorageTrieKey::new(addr_1, StoredNibbles(Nibbles::default())),
            StorageTrieKey::new(addr_1, StoredNibbles(Nibbles::from_nibbles_unchecked([0]))),
            StorageTrieKey::new(addr_1, StoredNibbles(Nibbles::from_nibbles_unchecked([0, 0, 15]))),
            StorageTrieKey::new(addr_1, StoredNibbles(Nibbles::from_nibbles_unchecked([0, 1]))),
            StorageTrieKey::new(addr_1, StoredNibbles(Nibbles::from_nibbles_unchecked([1]))),
            StorageTrieKey::new(addr_2, StoredNibbles(Nibbles::default())),
        ];

        for left in &keys {
            for right in &keys {
                assert_eq!(
                    left.cmp(right),
                    StorageTrieDomain::encode_key(left).cmp(&StorageTrieDomain::encode_key(right))
                );
            }
        }
    }

    #[test]
    fn history_key_orders_by_logical_key_then_block_suffix() {
        let short = StoredNibbles(Nibbles::from_nibbles_unchecked([5]));
        let long = StoredNibbles(Nibbles::from_nibbles_unchecked([1, 5]));
        assert!(
            encode_history_key::<AccountTrieDomain>(&long, 0)
                < encode_history_key::<AccountTrieDomain>(&short, 0)
        );
        assert!(
            encode_history_key::<AccountTrieDomain>(&short, 1)
                < encode_history_key::<AccountTrieDomain>(&short, 2)
        );

        let address = B256::repeat_byte(0x44);
        let short = StorageTrieKey::new(address, short);
        let long = StorageTrieKey::new(address, long);
        assert!(
            encode_history_key::<StorageTrieDomain>(&long, 0)
                < encode_history_key::<StorageTrieDomain>(&short, 0)
        );
        assert!(
            encode_history_key::<StorageTrieDomain>(&short, 1)
                < encode_history_key::<StorageTrieDomain>(&short, 2)
        );
    }

    #[test]
    fn opens_empty_v2_database() {
        let dir = TempDir::new().unwrap();
        let storage = RocksdbProofsStorage::new(dir.path()).unwrap();
        assert!(storage.get_earliest_block_number().unwrap().is_none());
        let cf = storage.cf(CF_METADATA).unwrap();
        let version =
            storage.db.get_cf(&cf, V2_SCHEMA_VERSION_KEY).map_err(rocksdb_error).unwrap().unwrap();
        assert_eq!(version.as_slice(), V2_SCHEMA_VERSION);
    }

    #[test]
    fn opening_legacy_v1_database_requires_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = LEGACY_COLUMN_FAMILIES
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(*name, Options::default()));
        let db = RocksDb::open_cf_descriptors(&options, dir.path(), descriptors).unwrap();
        drop(db);

        let error = RocksdbProofsStorage::new(dir.path()).unwrap_err();
        assert!(error.to_string().contains("legacy RocksDB proof-history database"), "{error}");
    }

    #[test]
    fn opening_mixed_legacy_and_v2_database_requires_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = [CF_METADATA, "ProofWindow"]
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
        let db = RocksDb::open_cf_descriptors(&options, dir.path(), descriptors).unwrap();
        drop(db);

        let error = RocksdbProofsStorage::new(dir.path()).unwrap_err();
        assert!(error.to_string().contains("legacy RocksDB proof-history database"), "{error}");
    }

    #[test]
    fn opening_database_with_unknown_v2_schema_version_fails() {
        let dir = TempDir::new().unwrap();
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = RocksdbProofsStorage::column_families()
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
        let db = RocksDb::open_cf_descriptors(&options, dir.path(), descriptors).unwrap();
        let cf = db.cf_handle(CF_METADATA).unwrap();
        db.put_cf(&cf, V2_SCHEMA_VERSION_KEY, b"unknown-version").unwrap();
        drop(cf);
        drop(db);

        let error = RocksdbProofsStorage::new(dir.path()).unwrap_err();
        assert!(error.to_string().contains("unsupported RocksDB proofs schema version"), "{error}");
    }

    #[test]
    fn incomplete_proof_window_metadata_is_rejected() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(42, B256::repeat_byte(0x42)).unwrap();

        assert_eq!(
            storage.get_earliest_block_number().unwrap(),
            Some((42, B256::repeat_byte(0x42)))
        );
        assert_eq!(storage.get_latest_block_number().unwrap(), None);
        let error = storage.get_proof_window().unwrap_err();
        assert!(error.to_string().contains("incomplete RocksDB proof window metadata"), "{error}");
    }

    #[test]
    fn v2_append_writes_current_state_and_history_before_rows() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(0, B256::ZERO).unwrap();

        let account_path = Nibbles::from_nibbles_unchecked([1, 2, 3]);
        let storage_path = Nibbles::from_nibbles_unchecked([4, 5, 6]);
        let hashed_address = B256::repeat_byte(0xAA);
        let hashed_storage_key = B256::repeat_byte(0xBB);
        let account_key = StoredNibbles(account_path);
        let storage_key = StorageTrieKey::new(hashed_address, StoredNibbles(storage_path));
        let hashed_storage = HashedStorageKey::new(hashed_address, hashed_storage_key);

        let block_1 = block(B256::ZERO, 1, 1);
        let block_2 = block(block_1.block.hash, 2, 2);
        let account_1 = account(1);
        let account_2 = account(2);
        let account_node_1 = branch(0x11);
        let account_node_2 = branch(0x22);
        let storage_node_1 = branch(0x33);
        let storage_node_2 = branch(0x44);

        storage
            .store_trie_updates(
                block_1,
                full_diff(
                    account_path,
                    Some(account_node_1.clone()),
                    hashed_address,
                    storage_path,
                    Some(storage_node_1.clone()),
                    Some(account_1),
                    hashed_storage_key,
                    U256::from(111),
                ),
            )
            .unwrap();
        storage
            .store_trie_updates(
                block_2,
                full_diff(
                    account_path,
                    Some(account_node_2.clone()),
                    hashed_address,
                    storage_path,
                    Some(storage_node_2.clone()),
                    Some(account_2),
                    hashed_storage_key,
                    U256::from(222),
                ),
            )
            .unwrap();

        assert_eq!(
            storage.read_current::<AccountTrieDomain>(&account_key).unwrap(),
            Some(account_node_2.clone())
        );
        assert_eq!(
            storage.read_current::<StorageTrieDomain>(&storage_key).unwrap(),
            Some(storage_node_2.clone())
        );
        assert_eq!(
            storage.read_current::<HashedAccountDomain>(&hashed_address).unwrap(),
            Some(account_2)
        );
        assert_eq!(
            storage.read_current::<HashedStorageDomain>(&hashed_storage).unwrap(),
            Some(StorageValue(U256::from(222)))
        );

        let snapshot = storage.db.snapshot();
        assert_eq!(
            storage.read_history_exact::<AccountTrieDomain>(&snapshot, &account_key, 1).unwrap(),
            Some(None)
        );
        assert_eq!(
            storage.read_history_exact::<AccountTrieDomain>(&snapshot, &account_key, 2).unwrap(),
            Some(Some(account_node_1.clone()))
        );
        assert_eq!(
            storage.read_history_exact::<StorageTrieDomain>(&snapshot, &storage_key, 2).unwrap(),
            Some(Some(storage_node_1.clone()))
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 2)
                .unwrap(),
            Some(Some(account_1))
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedStorageDomain>(&snapshot, &hashed_storage, 2)
                .unwrap(),
            Some(Some(StorageValue(U256::from(111))))
        );

        assert_eq!(
            storage.value_at::<HashedAccountDomain>(&snapshot, &hashed_address, 1).unwrap(),
            Some(account_1)
        );
        assert_eq!(
            storage.value_at::<HashedStorageDomain>(&snapshot, &hashed_storage, 1).unwrap(),
            Some(StorageValue(U256::from(111)))
        );
    }

    #[test]
    fn initial_state_writes_current_state_without_history_or_changesets() {
        let (storage, _dir) = temp_storage();
        let anchor = BlockNumHash::new(10, B256::repeat_byte(0x10));
        let account_path = Nibbles::from_nibbles_unchecked([1, 0]);
        let storage_path = Nibbles::from_nibbles_unchecked([2, 0]);
        let hashed_address = B256::repeat_byte(0xA1);
        let hashed_storage_key = B256::repeat_byte(0xB1);
        let account_node = branch(0x10);
        let storage_node = branch(0x20);
        let account = account(7);

        storage.set_initial_state_anchor(anchor).unwrap();
        storage.store_account_branches(vec![(account_path, Some(account_node.clone()))]).unwrap();
        storage
            .store_storage_branches(
                hashed_address,
                vec![(storage_path, Some(storage_node.clone()))],
            )
            .unwrap();
        storage.store_hashed_accounts(vec![(hashed_address, Some(account))]).unwrap();
        storage
            .store_hashed_storages(hashed_address, vec![(hashed_storage_key, U256::from(77))])
            .unwrap();
        storage.commit_initial_state().unwrap();

        let account_key = StoredNibbles(account_path);
        let storage_key = StorageTrieKey::new(hashed_address, StoredNibbles(storage_path));
        let hashed_storage = HashedStorageKey::new(hashed_address, hashed_storage_key);
        let snapshot = storage.db.snapshot();

        assert_eq!(storage.get_earliest_block_number().unwrap(), Some((10, anchor.hash)));
        assert_eq!(storage.get_latest_block_number().unwrap(), Some((10, anchor.hash)));
        assert_eq!(
            storage.read_current::<AccountTrieDomain>(&account_key).unwrap(),
            Some(account_node)
        );
        assert_eq!(
            storage.read_current::<StorageTrieDomain>(&storage_key).unwrap(),
            Some(storage_node)
        );
        assert_eq!(
            storage.read_current::<HashedAccountDomain>(&hashed_address).unwrap(),
            Some(account)
        );
        assert_eq!(
            storage.read_current::<HashedStorageDomain>(&hashed_storage).unwrap(),
            Some(StorageValue(U256::from(77)))
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 10)
                .unwrap(),
            None
        );
        assert!(storage.get_change_set_from_snapshot(&snapshot, 10).unwrap().is_none());
    }

    #[test]
    fn prune_deletes_exact_history_and_changeset_rows_without_rewriting_current_state() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(0, B256::ZERO).unwrap();

        let hashed_address = B256::repeat_byte(0xC1);
        let block_1 = block(B256::ZERO, 1, 1);
        let block_2 = block(block_1.block.hash, 2, 2);
        let block_3 = block(block_2.block.hash, 3, 3);
        let account_1 = account(1);
        let account_2 = account(2);
        let account_3 = account(3);

        storage.store_trie_updates(block_1, account_diff(hashed_address, Some(account_1))).unwrap();
        storage.store_trie_updates(block_2, account_diff(hashed_address, Some(account_2))).unwrap();
        storage.store_trie_updates(block_3, account_diff(hashed_address, Some(account_3))).unwrap();

        let snapshot = storage.db.snapshot();
        assert!(storage.get_change_set_from_snapshot(&snapshot, 1).unwrap().is_some());
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 2)
                .unwrap(),
            Some(Some(account_1))
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 3)
                .unwrap(),
            Some(Some(account_2))
        );

        let counts = storage.prune_earliest_state(block_2).unwrap();
        assert_eq!(counts.hashed_accounts_written_total, 1);

        let snapshot = storage.db.snapshot();
        assert!(storage.get_change_set_from_snapshot(&snapshot, 1).unwrap().is_none());
        assert!(storage.get_change_set_from_snapshot(&snapshot, 2).unwrap().is_none());
        assert!(storage.get_change_set_from_snapshot(&snapshot, 3).unwrap().is_some());
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 1)
                .unwrap(),
            None
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 2)
                .unwrap(),
            None
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 3)
                .unwrap(),
            Some(Some(account_2))
        );
        assert_eq!(
            storage.read_current::<HashedAccountDomain>(&hashed_address).unwrap(),
            Some(account_3)
        );
        assert_eq!(
            storage.value_at::<HashedAccountDomain>(&snapshot, &hashed_address, 2).unwrap(),
            Some(account_2)
        );
    }

    #[test]
    fn prune_boundary_keeps_adjacent_keys_and_later_history_rows_in_every_domain() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(0, B256::ZERO).unwrap();

        let address_1 = B256::repeat_byte(0xA1);
        let address_2 = B256::repeat_byte(0xA2);
        let slot_1 = B256::repeat_byte(0xB1);
        let slot_2 = B256::repeat_byte(0xB2);
        let account_path_1 = Nibbles::from_nibbles_unchecked([1]);
        let account_path_2 = Nibbles::from_nibbles_unchecked([1, 0]);
        let storage_path_1 = Nibbles::from_nibbles_unchecked([2]);
        let storage_path_2 = Nibbles::from_nibbles_unchecked([2, 0]);

        let block_1 = block(B256::ZERO, 1, 1);
        let block_2 = block(block_1.block.hash, 2, 2);
        let block_3 = block(block_2.block.hash, 3, 3);
        let account_node_1 = branch(0x11);
        let account_node_2 = branch(0x12);
        let storage_node_1 = branch(0x21);
        let storage_node_2 = branch(0x22);
        let account_1 = account(1);
        let account_2 = account(2);

        storage
            .store_trie_updates(
                block_1,
                full_diff(
                    account_path_1,
                    Some(account_node_1.clone()),
                    address_1,
                    storage_path_1,
                    Some(storage_node_1.clone()),
                    Some(account_1),
                    slot_1,
                    U256::from(11),
                ),
            )
            .unwrap();
        storage
            .store_trie_updates(
                block_2,
                full_diff(
                    account_path_1,
                    Some(branch(0x13)),
                    address_1,
                    storage_path_1,
                    Some(branch(0x23)),
                    Some(account(3)),
                    slot_1,
                    U256::from(33),
                ),
            )
            .unwrap();
        storage
            .store_trie_updates(
                block_3,
                full_diff(
                    account_path_2,
                    Some(account_node_2.clone()),
                    address_2,
                    storage_path_2,
                    Some(storage_node_2.clone()),
                    Some(account_2),
                    slot_2,
                    U256::from(22),
                ),
            )
            .unwrap();

        storage.prune_earliest_state(block_2).unwrap();

        let snapshot = storage.db.snapshot();
        let account_key_1 = StoredNibbles(account_path_1);
        let account_key_2 = StoredNibbles(account_path_2);
        let storage_key_1 = StorageTrieKey::new(address_1, StoredNibbles(storage_path_1));
        let storage_key_2 = StorageTrieKey::new(address_2, StoredNibbles(storage_path_2));
        let hashed_storage_1 = HashedStorageKey::new(address_1, slot_1);
        let hashed_storage_2 = HashedStorageKey::new(address_2, slot_2);

        assert_eq!(
            storage.read_history_exact::<AccountTrieDomain>(&snapshot, &account_key_1, 2).unwrap(),
            None
        );
        assert_eq!(
            storage.read_history_exact::<StorageTrieDomain>(&snapshot, &storage_key_1, 2).unwrap(),
            None
        );
        assert_eq!(
            storage.read_history_exact::<HashedAccountDomain>(&snapshot, &address_1, 2).unwrap(),
            None
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedStorageDomain>(&snapshot, &hashed_storage_1, 2)
                .unwrap(),
            None
        );

        assert_eq!(
            storage.read_history_exact::<AccountTrieDomain>(&snapshot, &account_key_2, 3).unwrap(),
            Some(None)
        );
        assert_eq!(
            storage.read_history_exact::<StorageTrieDomain>(&snapshot, &storage_key_2, 3).unwrap(),
            Some(None)
        );
        assert_eq!(
            storage.read_history_exact::<HashedAccountDomain>(&snapshot, &address_2, 3).unwrap(),
            Some(None)
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedStorageDomain>(&snapshot, &hashed_storage_2, 3)
                .unwrap(),
            Some(None)
        );
    }

    #[test]
    fn unwind_restores_current_state_and_removes_unwound_history_rows() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(0, B256::ZERO).unwrap();

        let hashed_address = B256::repeat_byte(0xD1);
        let block_1 = block(B256::ZERO, 1, 1);
        let block_2 = block(block_1.block.hash, 2, 2);
        let block_3 = block(block_2.block.hash, 3, 3);
        let account_1 = account(1);
        let account_2 = account(2);
        let account_3 = account(3);

        storage.store_trie_updates(block_1, account_diff(hashed_address, Some(account_1))).unwrap();
        storage.store_trie_updates(block_2, account_diff(hashed_address, Some(account_2))).unwrap();
        storage.store_trie_updates(block_3, account_diff(hashed_address, Some(account_3))).unwrap();

        storage.unwind_history(block_3).unwrap();

        let snapshot = storage.db.snapshot();
        assert_eq!(storage.get_latest_block_number().unwrap(), Some((2, block_2.block.hash)));
        assert_eq!(
            storage.read_current::<HashedAccountDomain>(&hashed_address).unwrap(),
            Some(account_2)
        );
        assert!(storage.get_change_set_from_snapshot(&snapshot, 3).unwrap().is_none());
        assert!(storage.get_change_set_from_snapshot(&snapshot, 2).unwrap().is_some());
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 3)
                .unwrap(),
            None
        );
        assert_eq!(
            storage
                .read_history_exact::<HashedAccountDomain>(&snapshot, &hashed_address, 2)
                .unwrap(),
            Some(Some(account_1))
        );
    }

    #[test]
    fn replace_updates_records_history_for_in_batch_storage_wipe() {
        let (storage, _dir) = temp_storage();
        storage.set_earliest_block_number(0, B256::ZERO).unwrap();

        let hashed_address = B256::repeat_byte(0xE1);
        let hashed_storage_key = B256::repeat_byte(0xE2);
        let storage_key = HashedStorageKey::new(hashed_address, hashed_storage_key);
        let block_1 = block(B256::ZERO, 1, 1);
        let old_block_2 = block(block_1.block.hash, 2, 2);
        let replacement_block_2 = block(block_1.block.hash, 2, 0x12);
        let replacement_block_3 = block(replacement_block_2.block.hash, 3, 0x13);

        storage
            .store_trie_updates(block_1, account_diff(hashed_address, Some(account(1))))
            .unwrap();
        storage
            .store_trie_updates(old_block_2, account_diff(hashed_address, Some(account(2))))
            .unwrap();

        storage
            .replace_updates(
                block_1.block,
                vec![
                    (
                        replacement_block_2,
                        storage_diff(hashed_address, hashed_storage_key, U256::from(222)),
                    ),
                    (replacement_block_3, wiped_storage_diff(hashed_address)),
                ],
            )
            .unwrap();

        let snapshot = storage.db.snapshot();
        assert_eq!(
            storage.get_latest_block_number().unwrap(),
            Some((3, replacement_block_3.block.hash))
        );
        assert_eq!(storage.read_current::<HashedStorageDomain>(&storage_key).unwrap(), None);
        assert_eq!(
            storage.read_history_exact::<HashedStorageDomain>(&snapshot, &storage_key, 2).unwrap(),
            Some(None)
        );
        assert_eq!(
            storage.read_history_exact::<HashedStorageDomain>(&snapshot, &storage_key, 3).unwrap(),
            Some(Some(StorageValue(U256::from(222))))
        );
        assert_eq!(
            storage.value_at::<HashedStorageDomain>(&snapshot, &storage_key, 2).unwrap(),
            Some(StorageValue(U256::from(222)))
        );
        assert_eq!(
            storage.value_at::<HashedStorageDomain>(&snapshot, &storage_key, 3).unwrap(),
            None
        );
    }
}
