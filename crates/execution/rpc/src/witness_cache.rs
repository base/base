//! Disk-backed ring of prebuilt `debug_executePayload` execution witnesses.

use std::{
    collections::{HashMap, hash_map},
    fs,
    io::{self, Write},
    num::NonZeroUsize,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_rpc_types_debug::ExecutionWitness;
use base_common_rpc_types_engine::BasePayloadAttributes;
use tracing::warn;

use crate::metrics::WitnessCacheMetrics;

/// File extension of committed witness cache entries.
const ENTRY_EXTENSION: &str = "witness.zst";

/// File extension of in-progress witness cache writes.
const TEMP_EXTENSION: &str = "tmp";

/// Makes temp file names unique, so concurrent inserts of the same entry (e.g. an RPC miss racing
/// the background builder) never write the same temp file.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Configuration for the prebuilt witness cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessCacheConfig {
    /// Directory holding one compressed file per cached witness.
    pub path: PathBuf,
    /// Number of blocks behind the proofs tip to keep cached witnesses for.
    pub retention_blocks: u64,
    /// Minimum number of blocks a block must be behind the proofs tip before it is prebuilt.
    pub build_lag: u64,
    /// Maximum number of witnesses built concurrently in the background.
    pub builder_concurrency: NonZeroUsize,
}

/// Index metadata of the cached witness of one block number.
///
/// The cache holds at most one entry per block number. It is only served for a request whose
/// parent hash and attributes digest both match, i.e. one that rebuilds the exact same payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessCacheEntry {
    /// Hash of the parent block the payload is built on.
    pub parent_hash: B256,
    /// Digest of the full payload attributes, see [`WitnessCache::attributes_digest`].
    pub attributes_digest: B256,
    /// Size of the compressed witness file in bytes.
    pub size: u64,
}

impl WitnessCacheEntry {
    /// Returns whether this entry holds the witness of a payload with the given parent hash and
    /// attributes digest.
    pub fn matches(&self, parent_hash: B256, attributes_digest: B256) -> bool {
        self.parent_hash == parent_hash && self.attributes_digest == attributes_digest
    }

    /// Returns the file name encoding this entry for block `block_number`.
    pub fn file_name(&self, block_number: u64) -> String {
        format!(
            "{block_number:020}-{:x}-{:x}.{ENTRY_EXTENSION}",
            self.parent_hash, self.attributes_digest
        )
    }

    /// Parses a file name produced by [`Self::file_name`] into the block number and entry;
    /// `size` is left as zero.
    pub fn parse_file_name(name: &str) -> Option<(u64, Self)> {
        let stem = name.strip_suffix(ENTRY_EXTENSION)?.strip_suffix('.')?;
        let mut parts = stem.split('-');
        let block_number = parts.next()?.parse().ok()?;
        let parent_hash = B256::from_str(parts.next()?).ok()?;
        let attributes_digest = B256::from_str(parts.next()?).ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some((block_number, Self { parent_hash, attributes_digest, size: 0 }))
    }
}

/// Bounded on-disk ring of execution witnesses, one zstd-compressed file per block number.
///
/// The in-memory index is rebuilt from the directory on [`Self::open`], so entries survive
/// restarts. Writes are atomic (temp file + rename). Entries are replaced by inserting another
/// witness for the same block number and removed by [`Self::evict_before`].
#[derive(Debug)]
pub struct WitnessCache {
    dir: PathBuf,
    entries: Mutex<HashMap<u64, WitnessCacheEntry>>,
}

impl WitnessCache {
    /// Opens the cache at `dir`, creating it if needed and indexing existing entries.
    ///
    /// Leftover temporary files from interrupted writes and entry files with unrecognized names
    /// (e.g. from an older file format) are removed. If several files exist for one block number,
    /// only the most recently modified one is kept.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;

        let mut found: HashMap<u64, (WitnessCacheEntry, SystemTime, PathBuf)> = HashMap::new();
        for dir_entry in fs::read_dir(&dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
            if path.extension().is_some_and(|ext| ext == TEMP_EXTENSION) {
                fs::remove_file(&path)?;
                continue;
            }
            if !name.ends_with(ENTRY_EXTENSION) {
                continue;
            }
            let Some((block_number, mut entry)) = WitnessCacheEntry::parse_file_name(name) else {
                Self::remove_file(&path);
                continue;
            };
            let metadata = dir_entry.metadata()?;
            entry.size = metadata.len();
            let modified = metadata.modified()?;
            match found.entry(block_number) {
                hash_map::Entry::Vacant(slot) => {
                    slot.insert((entry, modified, path));
                }
                hash_map::Entry::Occupied(mut slot) => {
                    if modified > slot.get().1 {
                        let (_, _, stale) = slot.insert((entry, modified, path));
                        Self::remove_file(&stale);
                    } else {
                        Self::remove_file(&path);
                    }
                }
            }
        }

        let entries =
            found.into_iter().map(|(block_number, (entry, _, _))| (block_number, entry)).collect();
        Self::record_size_metrics(&entries);
        Ok(Self { dir, entries: Mutex::new(entries) })
    }

    /// Returns the keccak256 digest of the JSON serialization of `attributes`.
    ///
    /// Covers every attribute field, so two requests only share a cache entry if they build the
    /// exact same payload.
    pub fn attributes_digest(attributes: &BasePayloadAttributes) -> B256 {
        keccak256(serde_json::to_vec(attributes).expect("payload attributes serialize to JSON"))
    }

    /// Returns whether the witness of block `block_number` built on `parent_hash` with the given
    /// attributes digest is cached.
    pub fn contains(&self, block_number: u64, parent_hash: B256, attributes_digest: B256) -> bool {
        self.entries
            .lock()
            .expect("witness cache lock poisoned")
            .get(&block_number)
            .is_some_and(|entry| entry.matches(parent_hash, attributes_digest))
    }

    /// Returns the inclusive block range of cached witnesses.
    pub fn block_range(&self) -> Option<RangeInclusive<u64>> {
        let entries = self.entries.lock().expect("witness cache lock poisoned");
        let min = *entries.keys().min()?;
        let max = *entries.keys().max()?;
        Some(min..=max)
    }

    /// Returns the cached witness of block `block_number` if it was built on `parent_hash` with
    /// the given attributes digest.
    ///
    /// Unreadable entries are dropped from the index and treated as misses.
    pub fn get(
        &self,
        block_number: u64,
        parent_hash: B256,
        attributes_digest: B256,
    ) -> Option<ExecutionWitness> {
        let entry =
            *self.entries.lock().expect("witness cache lock poisoned").get(&block_number)?;
        if !entry.matches(parent_hash, attributes_digest) {
            return None;
        }

        let path = self.dir.join(entry.file_name(block_number));
        match fs::read(&path).and_then(|data| Self::decode_witness(&data)) {
            Ok(witness) => Some(witness),
            Err(error) => {
                warn!(error = %error, path = %path.display(), "dropping unreadable witness cache entry");
                self.remove(block_number, &entry);
                None
            }
        }
    }

    /// Returns the cached witness of block `block_number`, or the result of `build` on a miss.
    ///
    /// A built witness is cached only if `block_number` lies within the cached block range and
    /// `is_canonical` confirms the request rebuilds the canonical block, so requests for blocks
    /// the background builder skipped still populate the ring while arbitrary attributes cannot
    /// grow it beyond one entry per canonical block.
    pub async fn get_or_build<E>(
        self: &Arc<Self>,
        block_number: u64,
        parent_hash: B256,
        attributes_digest: B256,
        build: impl Future<Output = Result<ExecutionWitness, E>>,
        is_canonical: impl FnOnce() -> bool,
    ) -> Result<ExecutionWitness, E> {
        let cache = Arc::clone(self);
        match tokio::task::spawn_blocking(move || {
            cache.get(block_number, parent_hash, attributes_digest)
        })
        .await
        {
            Ok(Some(witness)) => {
                WitnessCacheMetrics::hits().increment(1);
                return Ok(witness);
            }
            Ok(None) => {}
            Err(error) => warn!(error = %error, "witness cache lookup failed"),
        }
        WitnessCacheMetrics::misses().increment(1);

        let witness = build.await?;
        if !self.block_range().is_some_and(|range| range.contains(&block_number)) || !is_canonical()
        {
            return Ok(witness);
        }

        let cache = Arc::clone(self);
        let witness = Arc::new(witness);
        let cached = Arc::clone(&witness);
        let inserted = tokio::task::spawn_blocking(move || {
            cache.insert(block_number, parent_hash, attributes_digest, &cached)
        })
        .await;
        match inserted {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(error = %error, block = block_number, "failed to cache witness")
            }
            Err(error) => {
                warn!(error = %error, block = block_number, "failed to cache witness")
            }
        }
        Ok(Arc::unwrap_or_clone(witness))
    }

    /// Atomically writes `witness` to disk and indexes it as the entry of block `block_number`,
    /// replacing any previous entry of that block (e.g. one built before a reorg).
    pub fn insert(
        &self,
        block_number: u64,
        parent_hash: B256,
        attributes_digest: B256,
        witness: &ExecutionWitness,
    ) -> io::Result<()> {
        let data = Self::encode_witness(witness)?;
        let entry = WitnessCacheEntry { parent_hash, attributes_digest, size: data.len() as u64 };
        let file_name = entry.file_name(block_number);
        let path = self.dir.join(&file_name);
        let temp_path = self.dir.join(format!(
            "{file_name}.{}.{TEMP_EXTENSION}",
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let mut file = fs::File::create(&temp_path)?;
        file.write_all(&data)?;
        file.sync_data()?;

        // Committing under the lock keeps the file of the indexed entry in place: otherwise a
        // concurrent insert for this block could replace this file name in the index and delete
        // it right after this rename.
        let mut entries = self.entries.lock().expect("witness cache lock poisoned");
        let result = self.commit(&mut entries, block_number, entry, &temp_path, &path);
        if result.is_err() {
            Self::remove_file(&temp_path);
        }
        Self::record_size_metrics(&entries);
        result
    }

    /// Moves `temp_path` to `path` and indexes it, first deleting the file of a replaced entry.
    /// The replaced entry stays indexed if its file can't be deleted, so it is never orphaned
    /// outside eviction and disk-usage accounting.
    fn commit(
        &self,
        entries: &mut HashMap<u64, WitnessCacheEntry>,
        block_number: u64,
        entry: WitnessCacheEntry,
        temp_path: &Path,
        path: &Path,
    ) -> io::Result<()> {
        if let Some(previous) = entries.get(&block_number) {
            let previous_path = self.dir.join(previous.file_name(block_number));
            if previous_path != path {
                match fs::remove_file(&previous_path) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                    _ => {
                        entries.remove(&block_number);
                    }
                }
            }
        }
        fs::rename(temp_path, path)?;
        entries.insert(block_number, entry);
        Ok(())
    }

    /// Removes all entries for blocks below `block_number`.
    ///
    /// Files are deleted without holding the index lock, so lookups are not blocked on disk I/O.
    /// An entry stays indexed until its file is gone, so a failed deletion is retried by the next
    /// eviction instead of leaving an unindexed file that reappears on restart.
    pub fn evict_before(&self, block_number: u64) {
        let expired: Vec<_> = self
            .entries
            .lock()
            .expect("witness cache lock poisoned")
            .iter()
            .filter(|(block, _)| **block < block_number)
            .map(|(block, entry)| (*block, *entry))
            .collect();
        if expired.is_empty() {
            return;
        }

        let deleted: Vec<_> = expired
            .into_iter()
            .filter(|(block, entry)| Self::remove_file(&self.dir.join(entry.file_name(*block))))
            .collect();

        let mut entries = self.entries.lock().expect("witness cache lock poisoned");
        for (block, entry) in deleted {
            if entries.get(&block) == Some(&entry) {
                entries.remove(&block);
            }
        }
        Self::record_size_metrics(&entries);
    }

    /// Serializes and compresses a witness into the on-disk format.
    pub fn encode_witness(witness: &ExecutionWitness) -> io::Result<Vec<u8>> {
        let mut rlp = Vec::new();
        witness.state.encode(&mut rlp);
        witness.codes.encode(&mut rlp);
        witness.keys.encode(&mut rlp);
        witness.headers.encode(&mut rlp);
        zstd::encode_all(rlp.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
    }

    /// Decompresses and deserializes a witness from the on-disk format.
    pub fn decode_witness(data: &[u8]) -> io::Result<ExecutionWitness> {
        let rlp = zstd::decode_all(data)?;
        let buf = &mut rlp.as_slice();
        let decode = |buf: &mut &[u8]| {
            Vec::<Bytes>::decode(buf)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        };
        let witness = ExecutionWitness {
            state: decode(buf)?,
            codes: decode(buf)?,
            keys: decode(buf)?,
            headers: decode(buf)?,
        };
        if !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "trailing witness bytes"));
        }
        Ok(witness)
    }

    fn remove(&self, block_number: u64, entry: &WitnessCacheEntry) {
        let mut entries = self.entries.lock().expect("witness cache lock poisoned");
        if entries.get(&block_number) == Some(entry)
            && Self::remove_file(&self.dir.join(entry.file_name(block_number)))
        {
            entries.remove(&block_number);
            Self::record_size_metrics(&entries);
        }
    }

    /// Deletes a witness file, returning whether it is gone.
    fn remove_file(path: &Path) -> bool {
        match fs::remove_file(path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => {
                warn!(error = %error, path = %path.display(), "failed to remove witness cache entry");
                false
            }
        }
    }

    fn record_size_metrics(entries: &HashMap<u64, WitnessCacheEntry>) {
        WitnessCacheMetrics::entries().set(entries.len() as f64);
        WitnessCacheMetrics::bytes()
            .set(entries.values().map(|entry| entry.size).sum::<u64>() as f64);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::B64;

    use super::*;

    fn witness(seed: u8) -> ExecutionWitness {
        ExecutionWitness {
            state: vec![Bytes::from(vec![seed; 64]), Bytes::from(vec![seed + 1; 3])],
            codes: vec![Bytes::from(vec![seed; 100])],
            keys: vec![],
            headers: vec![Bytes::from(vec![seed; 500])],
        }
    }

    fn parent(seed: u8) -> B256 {
        B256::repeat_byte(seed)
    }

    fn file_name(block_number: u64, parent_hash: B256, attributes_digest: B256) -> String {
        WitnessCacheEntry { parent_hash, attributes_digest, size: 0 }.file_name(block_number)
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn get_requires_matching_parent_hash_and_attributes_digest() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        let digest = WitnessCache::attributes_digest(&BasePayloadAttributes::default());

        cache.insert(10, parent(1), digest, &witness(1)).unwrap();

        assert!(cache.contains(10, parent(1), digest));
        assert!(!cache.contains(10, parent(1), B256::ZERO));
        assert!(!cache.contains(10, parent(2), digest));
        assert_eq!(cache.get(10, parent(1), digest), Some(witness(1)));
        assert_eq!(cache.get(10, parent(1), B256::ZERO), None);
        assert_eq!(cache.get(10, parent(2), digest), None);
        assert_eq!(cache.get(11, parent(1), digest), None);
        assert_eq!(cache.block_range(), Some(10..=10));
    }

    #[test]
    fn insert_for_the_same_block_replaces_the_entry_and_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        cache.insert(10, parent(1), B256::ZERO, &witness(1)).unwrap();

        cache.insert(10, parent(2), B256::repeat_byte(9), &witness(2)).unwrap();

        assert_eq!(cache.get(10, parent(1), B256::ZERO), None);
        assert_eq!(cache.get(10, parent(2), B256::repeat_byte(9)), Some(witness(2)));
        assert_eq!(files(dir.path()), vec![file_name(10, parent(2), B256::repeat_byte(9))]);
    }

    #[test]
    fn insert_keeps_the_replaced_entry_when_its_file_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        cache.insert(10, parent(1), B256::ZERO, &witness(1)).unwrap();
        // A non-empty directory in place of the entry's file makes its deletion fail.
        let previous = dir.path().join(file_name(10, parent(1), B256::ZERO));
        fs::remove_file(&previous).unwrap();
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("keep"), b"").unwrap();

        assert!(cache.insert(10, parent(2), B256::repeat_byte(9), &witness(2)).is_err());

        assert_eq!(cache.block_range(), Some(10..=10));
        assert_eq!(cache.get(10, parent(2), B256::repeat_byte(9)), None);
        assert_eq!(files(dir.path()), vec![file_name(10, parent(1), B256::ZERO)]);
    }

    #[test]
    fn evict_before_removes_old_entries_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        for block in 1..=5u8 {
            cache.insert(block.into(), parent(block), B256::ZERO, &witness(block)).unwrap();
        }

        cache.evict_before(4);

        assert_eq!(cache.block_range(), Some(4..=5));
        assert!(!cache.contains(3, parent(3), B256::ZERO));
        assert_eq!(cache.get(4, parent(4), B256::ZERO), Some(witness(4)));
        assert_eq!(files(dir.path()).len(), 2);
    }

    #[test]
    fn reopen_rebuilds_index_and_removes_temp_and_unrecognized_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        let digest = B256::repeat_byte(7);
        {
            let cache = WitnessCache::open(dir.path()).unwrap();
            cache.insert(42, parent(1), digest, &witness(1)).unwrap();
            cache.insert(43, parent(2), digest, &witness(2)).unwrap();
        }
        let old_format =
            format!("{:020}-{:x}-{:x}-{digest:x}.{ENTRY_EXTENSION}", 44, parent(3), B64::ZERO);
        fs::write(dir.path().join(&old_format), b"old").unwrap();
        fs::write(dir.path().join("interrupted.tmp"), b"partial").unwrap();
        fs::write(dir.path().join("unrelated.txt"), b"ignored").unwrap();

        let cache = WitnessCache::open(dir.path()).unwrap();

        assert_eq!(cache.block_range(), Some(42..=43));
        assert_eq!(cache.get(42, parent(1), digest), Some(witness(1)));
        assert_eq!(cache.get(43, parent(2), digest), Some(witness(2)));
        assert_eq!(
            files(dir.path()),
            vec![
                file_name(42, parent(1), digest),
                file_name(43, parent(2), digest),
                "unrelated.txt".to_string(),
            ]
        );
    }

    #[test]
    fn reopen_keeps_only_the_most_recent_file_per_block() {
        let dir = tempfile::tempdir().unwrap();
        let data = |seed| WitnessCache::encode_witness(&witness(seed)).unwrap();
        let now = SystemTime::now();
        for (seed, age) in [(1u8, 20), (2, 0), (3, 10)] {
            let path = dir.path().join(file_name(5, parent(seed), B256::ZERO));
            fs::write(&path, data(seed)).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(age))
                .unwrap();
        }

        let cache = WitnessCache::open(dir.path()).unwrap();

        assert_eq!(cache.get(5, parent(2), B256::ZERO), Some(witness(2)));
        assert!(!cache.contains(5, parent(1), B256::ZERO));
        assert!(!cache.contains(5, parent(3), B256::ZERO));
        assert_eq!(files(dir.path()), vec![file_name(5, parent(2), B256::ZERO)]);
    }

    #[test]
    fn concurrent_inserts_for_one_block_leave_a_single_readable_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WitnessCache::open(dir.path()).unwrap());
        // Writers of the same entry share a file name, so without unique temp files they could
        // interleave into one temp file and commit a corrupt entry. Writers of different entries
        // replace each other's files.
        let big = |seed: u8| ExecutionWitness {
            state: (0..200).map(|i| Bytes::from(vec![seed ^ i; 4096])).collect(),
            ..witness(seed)
        };
        let writers = [(parent(1), 1u8), (parent(1), 2), (parent(2), 3)];
        std::thread::scope(|scope| {
            for (parent_hash, seed) in writers {
                let cache = Arc::clone(&cache);
                scope.spawn(move || {
                    for _ in 0..20 {
                        cache.insert(7, parent_hash, B256::ZERO, &big(seed)).unwrap();
                    }
                });
            }
        });

        let served: Vec<_> = writers
            .iter()
            .filter_map(|(parent_hash, seed)| {
                cache.get(7, *parent_hash, B256::ZERO).map(|cached| (*parent_hash, *seed, cached))
            })
            .collect();
        assert!(!served.is_empty());
        assert!(served.iter().all(|(parent_hash, _, cached)| {
            writers.iter().any(|(writer, seed)| writer == parent_hash && *cached == big(*seed))
        }));
        assert_eq!(files(dir.path()), vec![file_name(7, served[0].0, B256::ZERO)]);
    }

    #[test]
    fn insert_leaves_only_committed_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();

        cache.insert(1, parent(1), B256::ZERO, &witness(1)).unwrap();

        let entry =
            WitnessCacheEntry { parent_hash: parent(1), attributes_digest: B256::ZERO, size: 0 };
        assert_eq!(files(dir.path()), vec![entry.file_name(1)]);
        assert_eq!(WitnessCacheEntry::parse_file_name(&entry.file_name(1)), Some((1, entry)));
    }

    #[tokio::test]
    async fn get_or_build_serves_cached_witness_without_building() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WitnessCache::open(dir.path()).unwrap());
        cache.insert(10, parent(1), B256::ZERO, &witness(1)).unwrap();

        let mut built = false;
        let served = cache
            .get_or_build(
                10,
                parent(1),
                B256::ZERO,
                async {
                    built = true;
                    Ok::<_, ()>(witness(2))
                },
                || true,
            )
            .await;

        assert_eq!(served, Ok(witness(1)));
        assert!(!built);
    }

    #[tokio::test]
    async fn get_or_build_caches_canonical_misses_inside_block_range_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WitnessCache::open(dir.path()).unwrap());
        cache.insert(10, parent(1), B256::ZERO, &witness(1)).unwrap();
        cache.insert(20, parent(2), B256::ZERO, &witness(2)).unwrap();

        let build = |seed| async move { Ok::<_, ()>(witness(seed)) };
        assert_eq!(
            cache.get_or_build(15, parent(3), B256::ZERO, build(3), || true).await,
            Ok(witness(3))
        );
        assert_eq!(
            cache.get_or_build(21, parent(4), B256::ZERO, build(4), || true).await,
            Ok(witness(4))
        );
        assert_eq!(
            cache.get_or_build(16, parent(5), B256::ZERO, async { Err(()) }, || true).await,
            Err(())
        );
        assert_eq!(
            cache.get_or_build(17, parent(6), B256::ZERO, build(6), || false).await,
            Ok(witness(6))
        );

        assert_eq!(cache.get(15, parent(3), B256::ZERO), Some(witness(3)));
        assert!(!cache.contains(21, parent(4), B256::ZERO));
        assert!(!cache.contains(16, parent(5), B256::ZERO));
        assert!(!cache.contains(17, parent(6), B256::ZERO));
    }

    #[tokio::test]
    async fn get_or_build_replaces_mismatching_entry_of_the_same_block() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WitnessCache::open(dir.path()).unwrap());
        cache.insert(10, parent(1), B256::ZERO, &witness(1)).unwrap();

        let served = cache
            .get_or_build(
                10,
                parent(2),
                B256::repeat_byte(1),
                async { Ok::<_, ()>(witness(2)) },
                || true,
            )
            .await;

        assert_eq!(served, Ok(witness(2)));
        assert_eq!(cache.get(10, parent(2), B256::repeat_byte(1)), Some(witness(2)));
        assert!(!cache.contains(10, parent(1), B256::ZERO));
        assert_eq!(files(dir.path()).len(), 1);
    }

    #[test]
    fn attributes_digest_covers_every_attribute() {
        let attributes = BasePayloadAttributes {
            transactions: Some(vec![Bytes::from_static(b"tx")]),
            gas_limit: Some(30_000_000),
            ..Default::default()
        };
        let digest = WitnessCache::attributes_digest(&attributes);

        let json = serde_json::to_string(&attributes).unwrap();
        let decoded: BasePayloadAttributes = serde_json::from_str(&json).unwrap();
        assert_eq!(WitnessCache::attributes_digest(&decoded), digest);

        let changed = [
            BasePayloadAttributes { gas_limit: Some(1), ..attributes.clone() },
            BasePayloadAttributes { min_base_fee: Some(1), ..attributes.clone() },
            BasePayloadAttributes { eip_1559_params: Some(B64::ZERO), ..attributes.clone() },
            BasePayloadAttributes { transactions: Some(vec![]), ..attributes },
        ];
        for changed in changed {
            assert_ne!(WitnessCache::attributes_digest(&changed), digest);
        }
    }

    #[test]
    fn failed_eviction_keeps_entry_indexed_until_its_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        cache.insert(1, parent(1), B256::ZERO, &witness(1)).unwrap();
        let path = dir.path().join(file_name(1, parent(1), B256::ZERO));
        // A non-empty directory in place of the entry file makes its deletion fail.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("blocker"), b"").unwrap();

        cache.evict_before(2);
        assert!(cache.contains(1, parent(1), B256::ZERO));

        fs::remove_dir_all(&path).unwrap();
        cache.evict_before(2);
        assert!(!cache.contains(1, parent(1), B256::ZERO));
    }

    #[test]
    fn corrupt_entry_is_dropped_as_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WitnessCache::open(dir.path()).unwrap();
        cache.insert(1, parent(1), B256::ZERO, &witness(1)).unwrap();
        fs::write(dir.path().join(file_name(1, parent(1), B256::ZERO)), b"garbage").unwrap();

        assert_eq!(cache.get(1, parent(1), B256::ZERO), None);
        assert!(!cache.contains(1, parent(1), B256::ZERO));
        assert!(files(dir.path()).is_empty());
    }
}
