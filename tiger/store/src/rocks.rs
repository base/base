//! The datadir's `rocksdb/` directory: storage v2 keeps five tables in RocksDB column families.
//!
//! - `TransactionHashNumbers`: transaction hash -> transaction number.
//! - `AccountsHistory`: `address ++ highest block (u64 BE)` -> the blocks that changed the
//!   account, as a serialized roaring treemap ([`BlockList`]).
//! - `StoragesHistory`: `address ++ slot ++ highest block (u64 BE)` -> the same for one slot.
//! - `BlockAccessLists`, `BlockAccessListBlockNumbers`: created for reth, unused by tiger.
//!
//! History lists are sharded at [`SHARD_SIZE`] blocks: every full shard's key ends with its
//! highest block, and the open last shard's key ends with `u64::MAX`, as reth writes them.
//! The options match reth's `RocksDBBuilder` at base/reth 2b20e42, so either client opens the
//! files the other wrote. Every write is one synced `WriteBatch`; under `TIGER_FAULT` each batch
//! counts as one write (see [`crate::Fault`]).

use std::path::Path;

use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, CompactionPri, DBCompressionType, Options, WriteBatch,
    WriteBufferManager, WriteOptions, DB,
};

use crate::{Fault, StoreError};

/// Block numbers per full history shard (reth's `NUM_OF_INDICES_IN_SHARD`).
pub const SHARD_SIZE: usize = 2_000;

/// A RocksDB-backed table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RocksTable {
    /// Transaction hash -> number.
    TransactionHashNumbers,
    /// Account history shards.
    AccountsHistory,
    /// Storage history shards.
    StoragesHistory,
    /// Block access lists (unused by tiger).
    BlockAccessLists,
    /// Block access list hashes (unused by tiger).
    BlockAccessListBlockNumbers,
}

impl RocksTable {
    /// Every table, in reth's registration order.
    pub const ALL: [Self; 5] = [
        Self::TransactionHashNumbers,
        Self::AccountsHistory,
        Self::StoragesHistory,
        Self::BlockAccessLists,
        Self::BlockAccessListBlockNumbers,
    ];

    /// The column family name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TransactionHashNumbers => "TransactionHashNumbers",
            Self::AccountsHistory => "AccountsHistory",
            Self::StoragesHistory => "StoragesHistory",
            Self::BlockAccessLists => "BlockAccessLists",
            Self::BlockAccessListBlockNumbers => "BlockAccessListBlockNumbers",
        }
    }
}

/// A RocksDB error.
pub type RocksError = rocksdb::Error;

/// The open `rocksdb/` directory.
#[derive(Debug)]
pub struct Rocks {
    db: DB,
}

const BLOCK_SIZE: usize = 16 * 1024;
const CACHE_SIZE: usize = 128 << 20;
const WRITE_BUFFER_SIZE: usize = 128 << 20;
const WRITE_BUFFER_MANAGER_SIZE: usize = 4 * 1024 * 1024 * 1024;
const BYTES_PER_SYNC: u64 = 1_048_576;
const BAL_MIN_BLOB_SIZE: u64 = 4 * 1024;
const BAL_BLOB_FILE_SIZE: u64 = 256 * 1024 * 1024;

fn table_options(cache: &Cache) -> BlockBasedOptions {
    let mut table = BlockBasedOptions::default();
    table.set_block_size(BLOCK_SIZE);
    table.set_cache_index_and_filter_blocks(true);
    table.set_pin_l0_filter_and_index_blocks_in_cache(true);
    table.set_block_cache(cache);
    table
}

fn column_family_options(table: Option<RocksTable>, cache: &Cache) -> Options {
    let mut options = Options::default();
    options.set_block_based_table_factory(&table_options(cache));
    options.set_level_compaction_dynamic_level_bytes(true);
    if table == Some(RocksTable::TransactionHashNumbers) {
        options.set_compression_type(DBCompressionType::None);
        options.set_bottommost_compression_type(DBCompressionType::None);
        return options;
    }
    options.set_compression_type(DBCompressionType::Lz4);
    options.set_bottommost_compression_type(DBCompressionType::Zstd);
    options.set_bottommost_zstd_max_train_bytes(0, true);
    options.set_write_buffer_size(WRITE_BUFFER_SIZE);
    if table == Some(RocksTable::BlockAccessLists) {
        options.set_enable_blob_files(true);
        options.set_min_blob_size(BAL_MIN_BLOB_SIZE);
        options.set_blob_file_size(BAL_BLOB_FILE_SIZE);
        options.set_blob_compression_type(DBCompressionType::Lz4);
    }
    options
}

/// reth's `select_max_open_files`: keep every file open only with a high descriptor limit.
fn max_open_files() -> i32 {
    let mut limit = rocksdb_rlimit();
    if limit == 0 {
        limit = 1024;
    }
    if limit >= 128 * 1024 { -1 } else { 512 }
}

fn rocksdb_rlimit() -> u64 {
    // /proc/self/limits: "Max open files  <soft>  <hard>  files".
    std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|l| l.starts_with("Max open files"))?;
            line.split_whitespace().nth(3)?.parse().ok()
        })
        .unwrap_or(0)
}

impl Rocks {
    /// Opens (creating if needed) `dir` with reth's options, keeping any column families reth or
    /// a later version added.
    pub fn open(dir: &Path) -> Result<Self, RocksError> {
        Fault::init();
        let cache = Cache::new_lru_cache(CACHE_SIZE);
        let mut options = Options::default();
        options.set_block_based_table_factory(&table_options(&cache));
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        options.set_max_background_jobs(6);
        options.set_bytes_per_sync(BYTES_PER_SYNC);
        options.set_write_buffer_manager(&WriteBufferManager::new_write_buffer_manager(WRITE_BUFFER_MANAGER_SIZE, true));
        options.set_bottommost_compression_type(DBCompressionType::Zstd);
        options.set_bottommost_zstd_max_train_bytes(0, true);
        options.set_compression_type(DBCompressionType::Lz4);
        options.set_compaction_pri(CompactionPri::MinOverlappingRatio);
        options.set_log_level(rocksdb::LogLevel::Info);
        options.set_max_open_files(max_open_files());
        options.set_wal_ttl_seconds(0);
        options.set_wal_size_limit_mb(0);
        let mut descriptors: Vec<ColumnFamilyDescriptor> = RocksTable::ALL
            .iter()
            .map(|t| ColumnFamilyDescriptor::new(t.name(), column_family_options(Some(*t), &cache)))
            .collect();
        if dir.join("CURRENT").exists() {
            for name in DB::list_cf(&options, dir)? {
                if name != rocksdb::DEFAULT_COLUMN_FAMILY_NAME && !RocksTable::ALL.iter().any(|t| t.name() == name) {
                    descriptors.push(ColumnFamilyDescriptor::new(name, column_family_options(None, &cache)));
                }
            }
        }
        let db = DB::open_cf_descriptors(&options, dir, descriptors)?;
        Ok(Self { db })
    }

    fn cf(&self, table: RocksTable) -> &rocksdb::ColumnFamily {
        self.db.cf_handle(table.name()).expect("column family opened")
    }

    /// The value at `key`.
    pub fn get(&self, table: RocksTable, key: &[u8]) -> Result<Option<Vec<u8>>, RocksError> {
        self.db.get_cf(self.cf(table), key)
    }

    /// The first entry with a key at or after `key`.
    pub fn seek(&self, table: RocksTable, key: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>, RocksError> {
        let mut iter = self.db.raw_iterator_cf(self.cf(table));
        iter.seek(key);
        if iter.valid() {
            return Ok(Some((iter.key().expect("valid").to_vec(), iter.value().expect("valid").to_vec())));
        }
        iter.status()?;
        Ok(None)
    }

    /// Entries with keys starting with `prefix`, in key order, at most `limit`.
    pub fn scan_prefix(&self, table: RocksTable, prefix: &[u8], limit: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>, RocksError> {
        let mut out = Vec::new();
        let mut iter = self.db.raw_iterator_cf(self.cf(table));
        iter.seek(prefix);
        while iter.valid() && out.len() < limit {
            let key = iter.key().expect("valid");
            if !key.starts_with(prefix) {
                break;
            }
            out.push((key.to_vec(), iter.value().expect("valid").to_vec()));
            iter.next();
        }
        iter.status()?;
        Ok(out)
    }

    /// Writes `batch` atomically and durably (WAL synced). Under `TIGER_FAULT`, one counted
    /// write.
    pub fn write(&self, batch: RocksBatch) -> Result<(), StoreError> {
        if batch.ops.is_empty() {
            return Ok(());
        }
        let mut write = WriteBatch::default();
        for (table, key, value) in &batch.ops {
            match value {
                Some(value) => write.put_cf(self.cf(*table), key, value),
                None => write.delete_cf(self.cf(*table), key),
            }
        }
        let mut options = WriteOptions::default();
        options.set_sync(true);
        let mut result = Ok(());
        Fault::opaque_write("rocksdb", || result = self.db.write_opt(write, &options))?;
        Ok(result?)
    }
}

/// Puts and deletes for one [`Rocks::write`], applied in order.
#[derive(Debug, Default)]
pub struct RocksBatch {
    /// `(table, key, Some(value) to put or None to delete)`.
    pub ops: Vec<(RocksTable, Vec<u8>, Option<Vec<u8>>)>,
}

impl RocksBatch {
    /// Queues a put.
    pub fn put(&mut self, table: RocksTable, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push((table, key, Some(value)));
    }

    /// Queues a delete.
    pub fn delete(&mut self, table: RocksTable, key: Vec<u8>) {
        self.ops.push((table, key, None));
    }
}

/// reth's `BlockNumberList`: a sorted set of block numbers serialized as a roaring treemap
/// (portable format: a `u64` LE count of 32-bit bitmaps, each a `u32` LE high half followed by
/// a standard roaring bitmap).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockList(pub Vec<u64>);

const COOKIE_NO_RUN: u32 = 12346;
const COOKIE_RUN: u32 = 12347;
const ARRAY_LIMIT: usize = 4096;

impl BlockList {
    /// Serializes as roaring writes it (array containers up to 4096 values, bitmaps above; no
    /// run containers, as reth never optimizes runs).
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.0.windows(2).all(|w| w[0] < w[1]), "block list must be strictly ascending");
        // Group by the high 32 bits, then by the next 16.
        let mut out = Vec::new();
        let highs: Vec<u32> = {
            let mut h: Vec<u32> = self.0.iter().map(|b| (b >> 32) as u32).collect();
            h.dedup();
            h
        };
        out.extend_from_slice(&(highs.len() as u64).to_le_bytes());
        for high in highs {
            out.extend_from_slice(&high.to_le_bytes());
            let lows: Vec<u32> = self.0.iter().filter(|b| (*b >> 32) as u32 == high).map(|b| *b as u32).collect();
            let mut containers: Vec<(u16, Vec<u16>)> = Vec::new();
            for low in lows {
                let key = (low >> 16) as u16;
                match containers.last_mut() {
                    Some((k, values)) if *k == key => values.push(low as u16),
                    _ => containers.push((key, vec![low as u16])),
                }
            }
            out.extend_from_slice(&COOKIE_NO_RUN.to_le_bytes());
            out.extend_from_slice(&(containers.len() as u32).to_le_bytes());
            for (key, values) in &containers {
                out.extend_from_slice(&key.to_le_bytes());
                out.extend_from_slice(&((values.len() - 1) as u16).to_le_bytes());
            }
            let mut offset = 8 + 8 * containers.len() as u32;
            for (_, values) in &containers {
                out.extend_from_slice(&offset.to_le_bytes());
                offset += if values.len() <= ARRAY_LIMIT { 2 * values.len() as u32 } else { 8192 };
            }
            for (_, values) in &containers {
                if values.len() <= ARRAY_LIMIT {
                    for v in values {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                } else {
                    let mut bits = [0u64; 1024];
                    for v in values {
                        bits[*v as usize / 64] |= 1 << (v % 64);
                    }
                    for word in bits {
                        out.extend_from_slice(&word.to_le_bytes());
                    }
                }
            }
        }
        out
    }

    /// Deserializes any portable roaring treemap (array, bitmap and run containers).
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], String> {
            let slice = bytes.get(at..at + n).ok_or("short block list")?;
            at += n;
            Ok(slice)
        };
        let u16_at = |s: &[u8], i: usize| u16::from_le_bytes([s[2 * i], s[2 * i + 1]]);
        let bitmaps = u64::from_le_bytes(take(8)?.try_into().expect("8"));
        let mut out = Vec::new();
        for _ in 0..bitmaps {
            let high = u32::from_le_bytes(take(4)?.try_into().expect("4")) as u64;
            let cookie = u32::from_le_bytes(take(4)?.try_into().expect("4"));
            let (count, runs) = if cookie == COOKIE_NO_RUN {
                (u32::from_le_bytes(take(4)?.try_into().expect("4")) as usize, Vec::new())
            } else if cookie & 0xffff == COOKIE_RUN {
                let count = (cookie >> 16) as usize + 1;
                (count, take(count.div_ceil(8))?.to_vec())
            } else {
                return Err(format!("bad roaring cookie {cookie}"));
            };
            let descriptions = take(4 * count)?.to_vec();
            if runs.is_empty() || count >= 4 {
                take(4 * count)?;
            }
            for c in 0..count {
                let key = u16_at(&descriptions, 2 * c) as u64;
                let cardinality = u16_at(&descriptions, 2 * c + 1) as usize + 1;
                let base = (high << 32) | (key << 16);
                let is_run = runs.get(c / 8).is_some_and(|b| b & (1 << (c % 8)) != 0);
                if is_run {
                    let n = u16::from_le_bytes(take(2)?.try_into().expect("2")) as usize;
                    let pairs = take(4 * n)?;
                    for r in 0..n {
                        let start = u16_at(pairs, 2 * r) as u64;
                        let len = u16_at(pairs, 2 * r + 1) as u64;
                        out.extend((start..=start + len).map(|v| base | v));
                    }
                } else if cardinality <= ARRAY_LIMIT {
                    let values = take(2 * cardinality)?;
                    out.extend((0..cardinality).map(|i| base | u16_at(values, i) as u64));
                } else {
                    let words = take(8192)?;
                    for (w, chunk) in words.chunks_exact(8).enumerate() {
                        let mut word = u64::from_le_bytes(chunk.try_into().expect("8"));
                        while word != 0 {
                            let bit = word.trailing_zeros() as u64;
                            out.push(base | (w as u64 * 64 + bit));
                            word &= word - 1;
                        }
                    }
                }
            }
        }
        if at != bytes.len() {
            return Err("trailing bytes in block list".into());
        }
        if !out.windows(2).all(|w| w[0] < w[1]) {
            return Err("block list not ascending".into());
        }
        Ok(Self(out))
    }
}

/// History index maintenance over [`Rocks`] (`AccountsHistory`, `StoragesHistory`).
#[derive(Debug)]
pub struct History;

impl History {
    /// The shard key: `prefix` (address, or address ++ slot) then `highest` big-endian.
    pub fn key(prefix: &[u8], highest: u64) -> Vec<u8> {
        let mut key = Vec::with_capacity(prefix.len() + 8);
        key.extend_from_slice(prefix);
        key.extend_from_slice(&highest.to_be_bytes());
        key
    }

    /// All shards of `prefix`, oldest first: `(highest key block, blocks)`.
    pub fn shards(rocks: &Rocks, table: RocksTable, prefix: &[u8]) -> Result<Vec<(u64, BlockList)>, String> {
        let entries = rocks.scan_prefix(table, prefix, usize::MAX).map_err(|e| e.to_string())?;
        let mut shards = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            if key.len() != prefix.len() + 8 {
                continue;
            }
            let highest = u64::from_be_bytes(key[prefix.len()..].try_into().expect("8"));
            shards.push((highest, BlockList::decode(&value)?));
        }
        Ok(shards)
    }

    /// Queues appending `blocks` (ascending, all above the stored ones) to `prefix`'s history.
    pub fn append(rocks: &Rocks, table: RocksTable, prefix: &[u8], blocks: &[u64], batch: &mut RocksBatch) -> Result<(), String> {
        if blocks.is_empty() {
            return Ok(());
        }
        let last_key = Self::key(prefix, u64::MAX);
        let mut list = match rocks.get(table, &last_key).map_err(|e| e.to_string())? {
            Some(value) => BlockList::decode(&value)?.0,
            None => Vec::new(),
        };
        if let (Some(last), Some(first)) = (list.last(), blocks.first()) {
            assert!(first > last, "history append must be ascending");
        }
        list.extend_from_slice(blocks);
        Self::put_shards(table, prefix, &list, batch);
        Ok(())
    }

    /// Queues `list` as full shards keyed by their highest block, then the last shard at
    /// `u64::MAX`.
    fn put_shards(table: RocksTable, prefix: &[u8], list: &[u64], batch: &mut RocksBatch) {
        let mut chunks = list.chunks(SHARD_SIZE).peekable();
        while let Some(chunk) = chunks.next() {
            let highest = if chunks.peek().is_some() { *chunk.last().expect("non-empty") } else { u64::MAX };
            batch.put(table, Self::key(prefix, highest), BlockList(chunk.to_vec()).encode());
        }
    }

    /// Queues removing every block above `last_block` from `prefix`'s history, leaving the
    /// newest remaining shard at `u64::MAX` as reth's unwind does.
    pub fn unwind(rocks: &Rocks, table: RocksTable, prefix: &[u8], last_block: u64, batch: &mut RocksBatch) -> Result<(), String> {
        let shards = Self::shards(rocks, table, prefix)?;
        let mut keep: Vec<u64> = Vec::new();
        for (highest, list) in shards.iter().rev() {
            batch.delete(table, Self::key(prefix, *highest));
            if list.0.first().is_some_and(|b| *b <= last_block) {
                keep = list.0.iter().copied().filter(|b| *b <= last_block).collect();
                break;
            }
        }
        if !keep.is_empty() {
            batch.put(table, Self::key(prefix, u64::MAX), BlockList(keep).encode());
        }
        Ok(())
    }

    /// The first block above `block` that changed `prefix`, if any.
    pub fn first_change_after(rocks: &Rocks, table: RocksTable, prefix: &[u8], block: u64) -> Result<Option<u64>, String> {
        let Some(target) = block.checked_add(1) else { return Ok(None) };
        let mut iter_key = Self::key(prefix, target);
        // Shards are keyed by their highest block, so the first shard at or after `target`
        // holds the answer if any shard does.
        loop {
            let Some((key, value)) = rocks.seek(table, &iter_key).map_err(|e| e.to_string())? else { return Ok(None) };
            if key.len() != prefix.len() + 8 || !key.starts_with(prefix) {
                return Ok(None);
            }
            let list = BlockList::decode(&value)?;
            if let Some(found) = list.0.iter().copied().find(|b| *b >= target) {
                return Ok(Some(found));
            }
            let highest = u64::from_be_bytes(key[prefix.len()..].try_into().expect("8"));
            match highest.checked_add(1) {
                Some(next) => iter_key = Self::key(prefix, next),
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_lists_round_trip_and_match_roaring_layout() {
        // One array container: cookie, count 1, key 0 card-1 2, offset 16, values.
        let list = BlockList(vec![1, 5, 9]);
        let bytes = list.encode();
        let mut expected = 1u64.to_le_bytes().to_vec();
        expected.extend_from_slice(&0u32.to_le_bytes());
        expected.extend_from_slice(&12346u32.to_le_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&[0, 0, 2, 0]);
        expected.extend_from_slice(&16u32.to_le_bytes());
        expected.extend_from_slice(&[1, 0, 5, 0, 9, 0]);
        assert_eq!(bytes, expected);
        for list in [
            vec![],
            vec![0, 65_535, 65_536, 1 << 32, (1 << 32) + 7, u64::MAX - 1],
            (0..5_000u64).map(|i| i * 3).collect(),
            (100..10_000u64).collect(),
        ] {
            let list = BlockList(list);
            assert_eq!(BlockList::decode(&list.encode()).unwrap(), list);
        }
        // A run container: cookie 12347 with count-1 = 0 in the high half, run bitmap 1.
        let mut run = 1u64.to_le_bytes().to_vec();
        run.extend_from_slice(&0u32.to_le_bytes());
        run.extend_from_slice(&12347u32.to_le_bytes());
        run.push(1);
        run.extend_from_slice(&[0, 0, 9, 0]);
        run.extend_from_slice(&[1, 0, 10, 0, 9, 0]);
        assert_eq!(BlockList::decode(&run).unwrap().0, (10..=19).collect::<Vec<_>>());
    }

    #[test]
    fn history_shards_append_unwind_and_lookup() {
        let dir = std::env::temp_dir().join(format!("tiger-rocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rocks = Rocks::open(&dir).unwrap();
        let table = RocksTable::AccountsHistory;
        let prefix = [7u8; 20];
        let all: Vec<u64> = (0..4_500u64).map(|i| i * 2 + 10).collect();
        for chunk in all.chunks(700) {
            let mut batch = RocksBatch::default();
            History::append(&rocks, table, &prefix, chunk, &mut batch).unwrap();
            rocks.write(batch).unwrap();
        }
        let shards = History::shards(&rocks, table, &prefix).unwrap();
        assert_eq!(shards.len(), 3);
        assert_eq!(shards[0].0, all[1_999]);
        assert_eq!(shards[2].0, u64::MAX);
        assert_eq!(shards.iter().flat_map(|(_, l)| l.0.clone()).collect::<Vec<_>>(), all);
        assert_eq!(History::first_change_after(&rocks, table, &prefix, 0).unwrap(), Some(10));
        assert_eq!(History::first_change_after(&rocks, table, &prefix, 10).unwrap(), Some(12));
        assert_eq!(History::first_change_after(&rocks, table, &prefix, all[1_999]).unwrap(), Some(all[2_000]));
        assert_eq!(History::first_change_after(&rocks, table, &prefix, *all.last().unwrap()).unwrap(), None);
        assert_eq!(History::first_change_after(&rocks, table, &[8u8; 20], 0).unwrap(), None);
        // Unwind into the first shard.
        let mut batch = RocksBatch::default();
        History::unwind(&rocks, table, &prefix, all[1_500], &mut batch).unwrap();
        rocks.write(batch).unwrap();
        let shards = History::shards(&rocks, table, &prefix).unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0], (u64::MAX, BlockList(all[..=1_500].to_vec())));
        drop(rocks);
        let rocks = Rocks::open(&dir).unwrap();
        assert_eq!(History::first_change_after(&rocks, table, &prefix, all[1_500]).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reads a base-reth datadir's history (set `TIGER_RETH_DATADIR`); every value re-encodes to
    /// the same bytes.
    #[test]
    fn reads_a_reth_datadir() {
        let Ok(datadir) = std::env::var("TIGER_RETH_DATADIR") else { return };
        let rocks = Rocks::open(&Path::new(&datadir).join("rocksdb")).unwrap();
        let mut seen = 0;
        for table in [RocksTable::AccountsHistory, RocksTable::StoragesHistory] {
            for (key, value) in rocks.scan_prefix(table, &[], usize::MAX).unwrap() {
                let list = BlockList::decode(&value).unwrap();
                assert_eq!(list.encode(), value, "{key:?}");
                seen += 1;
            }
        }
        assert!(seen > 0);
        let txs = rocks.scan_prefix(RocksTable::TransactionHashNumbers, &[], 3).unwrap();
        assert!(!txs.is_empty());
        for (key, value) in txs {
            assert_eq!(key.len(), 32);
            eprintln!("tx number value {value:?}");
        }
    }
}
