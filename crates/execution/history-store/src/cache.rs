//! Reader segment cache: a small always-on memory tier and an optional on-disk LRU (TDD §5).
//!
//! reth's static-file reader memory-maps files and opens each segment's jars lazily, by path, on
//! first use, so a loaded segment needs its directory for as long as it lives. The memory tier
//! keeps a segment's files in a temporary directory removed when the segment is dropped. With a
//! cache directory configured, fetched segments persist there within a byte budget, and every
//! loaded segment holds a lease that keeps its directory from being evicted. Correctness never
//! depends on either tier: a miss re-downloads from the bucket and every fill is checked against
//! the segment's checksum manifest.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    num::NonZeroUsize,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use alloy_primitives::Bytes;
use base_common_consensus::BasePrimitives;
use lru::LruCache;
use reth_provider::providers::{StaticFileProvider, StaticFileProviderBuilder};
use tempfile::TempDir;

use crate::{
    BodyIndicesFile, Catalog, HistoryStoreError, IndexReader, SegmentFile, SegmentFiles,
    SegmentManifest, SegmentObjects,
};

/// File touched on every disk-tier hit; its mtime orders LRU eviction.
const USED_MARKER: &str = "USED";

/// Prefix of in-progress fill directories, renamed into place once complete.
const FILL_PREFIX: &str = "fill-";

/// Prefix of published disk entries, followed by the segment number.
const ENTRY_PREFIX: &str = "seg-";

/// Cache sizing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    /// Segments kept open in memory.
    pub memory_segments: NonZeroUsize,
    /// Optional disk cache directory (`--archive.cache-dir`); off by default.
    pub disk_dir: Option<PathBuf>,
    /// Byte budget of the disk cache (`--archive.cache-size`).
    pub disk_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_segments: NonZeroUsize::new(4).expect("non-zero"),
            disk_dir: None,
            disk_bytes: 1 << 30,
        }
    }
}

/// Keeps a published disk entry from being evicted while a [`LoadedSegment`] may still open
/// jars from it. The cache tracks leases weakly; the entry becomes evictable once the last
/// segment holding it is dropped.
#[derive(Debug)]
pub struct DiskLease {
    /// Segment number.
    pub segment: u64,
    /// Entry directory.
    pub dir: PathBuf,
}

/// Directory backing a [`LoadedSegment`].
#[derive(Debug)]
pub enum SegmentDir {
    /// A temporary directory removed when the segment is dropped.
    Temp(TempDir),
    /// A leased disk-tier entry.
    Disk(Arc<DiskLease>),
}

/// One segment opened for reads.
#[derive(Debug)]
pub struct LoadedSegment {
    /// Segment number.
    pub segment: u64,
    /// First block of the segment.
    pub first_block: u64,
    /// reth read-only static-file provider over the segment's files. Declared before `dir` so
    /// it is dropped first.
    pub provider: StaticFileProvider<BasePrimitives>,
    /// Decoded `block_bodies.idx`.
    pub bodies: BodyIndicesFile,
    /// Directory the provider reads from, kept alive as long as the segment.
    pub dir: SegmentDir,
}

impl LoadedSegment {
    /// Opens the segment whose static files are already in `path`, backed by `dir`.
    pub fn open(
        segment: u64,
        blocks_per_segment: u64,
        path: &Path,
        bodies: BodyIndicesFile,
        dir: SegmentDir,
    ) -> Result<Self, HistoryStoreError> {
        let provider = std::panic::catch_unwind(AssertUnwindSafe(|| {
            StaticFileProviderBuilder::read_only(path)
                .with_blocks_per_file(blocks_per_segment)
                .build::<BasePrimitives>()
        }))
        .map_err(|_| HistoryStoreError::CorruptSegment {
            segment,
            file: "static files".to_string(),
            reason: "static file provider panicked while opening".to_string(),
        })??;
        Ok(Self { segment, first_block: segment * blocks_per_segment, provider, bodies, dir })
    }
}

/// Disk-tier bookkeeping shared by every fill of one cache, guarded by one lock so admission,
/// eviction and lease registration never interleave.
#[derive(Debug, Default)]
pub struct DiskState {
    /// Leases on published entries, by segment.
    pub leases: HashMap<u64, Weak<DiskLease>>,
    /// Bytes admitted for fills that have not published yet.
    pub reserved: u64,
}

impl DiskState {
    /// Returns the live lease on segment `k`'s entry, if any.
    pub fn live_lease(&self, k: u64) -> Option<Arc<DiskLease>> {
        self.leases.get(&k).and_then(Weak::upgrade)
    }

    /// Returns the live lease on `dir` for segment `k`, creating one if none is live.
    pub fn lease(&mut self, k: u64, dir: &Path) -> Arc<DiskLease> {
        if let Some(lease) = self.live_lease(k) {
            return lease;
        }
        self.leases.retain(|_, lease| lease.strong_count() > 0);
        let lease = Arc::new(DiskLease { segment: k, dir: dir.to_path_buf() });
        self.leases.insert(k, Arc::downgrade(&lease));
        lease
    }
}

/// Disk bytes admitted for one fill, returned to the budget on drop, including when the fill
/// future is cancelled.
#[derive(Debug)]
pub struct Reservation<'a> {
    /// Cache the bytes were admitted to.
    pub cache: &'a SegmentCache,
    /// Admitted bytes.
    pub bytes: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.cache.disk_state().reserved -= self.bytes;
    }
}

/// One caller's share of a segment's fill lock. The lock leaves the cache's fill map when the
/// last caller drops its share, including when a caller's future is cancelled mid-fill.
#[derive(Debug)]
pub struct FillShare<'a> {
    /// Cache holding the fill map.
    pub cache: &'a SegmentCache,
    /// Segment number.
    pub segment: u64,
    /// The segment's fill lock.
    pub lock: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> FillShare<'a> {
    /// Joins (or starts) the fill of segment `k`.
    pub fn join(cache: &'a SegmentCache, k: u64) -> Self {
        let lock = Arc::clone(
            cache.fills.lock().unwrap_or_else(PoisonError::into_inner).entry(k).or_default(),
        );
        Self { cache, segment: k, lock }
    }
}

impl Drop for FillShare<'_> {
    fn drop(&mut self) {
        let mut fills = self.cache.fills.lock().unwrap_or_else(PoisonError::into_inner);
        let last = fills.get(&self.segment).is_some_and(|entry| Arc::ptr_eq(entry, &self.lock))
            && Arc::strong_count(&self.lock) == 2;
        if last {
            fills.remove(&self.segment);
        }
    }
}

/// Two-tier segment cache in front of the bucket.
///
/// - **Integrity.** Every fill, from the bucket or from disk, is checked against the segment's
///   SHA-256 manifest from the index before the files are opened. A corrupt disk entry is
///   removed and refetched.
/// - **Namespacing.** Disk entries live under `<disk_dir>/<namespace>/seg-<k>`, where the
///   namespace is the archive identity (chain id, genesis hash, format, `N`). Archives with
///   another identity never share entries. Within one identity, segment bytes are
///   deterministic and manifest-checked, so two buckets of the same chain may share entries.
/// - **Single flight.** Concurrent misses for one segment wait on one fill. The fill writes
///   into a `fill-*` directory and renames it into place, so readers and other processes never
///   see a partial entry.
/// - **Leases.** Every segment loaded from the disk tier leases its entry, and eviction skips
///   leased entries, because reth opens jars lazily by path after the segment is loaded.
///   Segments held by the memory tier stay leased, so the budget must exceed
///   `memory_segments` segments for the disk tier to hold anything else.
/// - **Budget.** A fill reserves its segment's manifest size before downloading, under the
///   same lock as eviction, and keeps the reservation until its entry is published. Published
///   entries plus reservations never exceed `disk_bytes`; a fill that cannot be admitted
///   (larger than the budget, or blocked by leased entries and other fills) is served from a
///   temporary directory and never persisted. Leases and reservations are process-local: two
///   processes sharing one cache directory each enforce the budget for their own fills only.
/// - **Restart.** Opening the cache removes abandoned `fill-*` directories and evicts persisted
///   entries until they fit the current budget ([`Self::reconcile_disk`]), before any lease.
pub struct SegmentCache {
    /// Sizing.
    pub config: CacheConfig,
    /// Bucket access.
    pub objects: SegmentObjects,
    /// Blocks per segment.
    pub blocks_per_segment: u64,
    /// Archive identity directory name under `disk_dir`.
    pub namespace: String,
    /// Memory tier.
    pub memory: Mutex<LruCache<u64, Arc<LoadedSegment>>>,
    /// In-flight fills, one lock per segment.
    pub fills: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    /// Disk-tier leases and reservations.
    pub disk: Mutex<DiskState>,
    /// Number of bucket downloads performed.
    pub downloads: AtomicU64,
}

impl std::fmt::Debug for SegmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentCache")
            .field("config", &self.config)
            .field("namespace", &self.namespace)
            .field("memory_len", &self.memory_len())
            .finish_non_exhaustive()
    }
}

impl SegmentCache {
    /// Creates a cache for the archive described by `catalog`.
    pub fn new(
        config: CacheConfig,
        objects: SegmentObjects,
        catalog: &Catalog,
    ) -> Result<Self, HistoryStoreError> {
        let blocks_per_segment = catalog.blocks_per_segment()?;
        let namespace = format!(
            "{}-{}-f{}-n{blocks_per_segment}",
            catalog.chain_id, catalog.genesis_hash, catalog.format
        );
        let memory = Mutex::new(LruCache::new(config.memory_segments));
        let cache = Self {
            config,
            objects,
            blocks_per_segment,
            namespace,
            memory,
            fills: Mutex::default(),
            disk: Mutex::default(),
            downloads: AtomicU64::new(0),
        };
        cache.reconcile_disk()?;
        Ok(cache)
    }

    /// Brings a persisted disk tier back within this cache's budget before any lease exists.
    ///
    /// A `fill-*` directory left by a crashed process is never valid (publication is an atomic
    /// rename), and its bytes are not covered by any reservation, so it is removed. Entries
    /// written under a larger budget are then evicted least recently used first until the
    /// published entries fit; an entry larger than the whole budget is removed, so later reads
    /// of that segment follow the ephemeral policy instead of leasing it in place.
    pub fn reconcile_disk(&self) -> Result<(), HistoryStoreError> {
        let Some(root) = self.disk_root().filter(|root| root.exists()) else { return Ok(()) };
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let abandoned = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(FILL_PREFIX));
            if abandoned {
                tracing::info!(path = %path.display(), "removing abandoned cache fill");
                fs::remove_dir_all(&path)?;
            }
        }
        let entries = Self::disk_entries(&root)?;
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        // Once the total fits, every remaining entry fits too, including against a budget
        // smaller than one entry, which this loop empties.
        for (_, size, path) in entries {
            if total <= self.config.disk_bytes {
                break;
            }
            tracing::info!(path = %path.display(), size, "evicting cache entry over the budget");
            fs::remove_dir_all(&path)?;
            total -= size;
        }
        Ok(())
    }

    /// Number of segments in the memory tier.
    pub fn memory_len(&self) -> usize {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Directory holding this archive's disk entries, if the disk tier is on.
    pub fn disk_root(&self) -> Option<PathBuf> {
        self.config.disk_dir.as_ref().map(|dir| dir.join(&self.namespace))
    }

    /// Locks the disk-tier state.
    pub fn disk_state(&self) -> std::sync::MutexGuard<'_, DiskState> {
        self.disk.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bytes admitted for fills that have not published yet.
    pub fn disk_reserved(&self) -> u64 {
        self.disk_state().reserved
    }

    /// Returns segment `k`, loading it from disk or the bucket on a miss. `index` supplies the
    /// checksum manifest the fill is verified against.
    pub async fn get(
        &self,
        k: u64,
        index: &IndexReader,
    ) -> Result<Arc<LoadedSegment>, HistoryStoreError> {
        if let Some(hit) = self.memory_hit(k) {
            return Ok(hit);
        }
        let share = FillShare::join(self, k);
        let _fill = share.lock.lock().await;
        if let Some(hit) = self.memory_hit(k) {
            return Ok(hit);
        }
        let loaded = Arc::new(self.load(k, index).await?);
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).put(k, Arc::clone(&loaded));
        Ok(loaded)
    }

    /// Returns segment `k` from the memory tier.
    pub fn memory_hit(&self, k: u64) -> Option<Arc<LoadedSegment>> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).get(&k).map(Arc::clone)
    }

    /// Loads segment `k` from the disk tier or the bucket, verified against its manifest.
    pub async fn load(&self, k: u64, index: &IndexReader) -> Result<LoadedSegment, HistoryStoreError> {
        let n = self.blocks_per_segment;
        let manifest = index.manifest(k).await?;
        let Some(root) = self.disk_root() else { return self.load_ephemeral(&manifest).await };
        let dir = root.join(format!("{ENTRY_PREFIX}{k}"));
        if let Some(lease) = self.lease_existing(k, &dir) {
            match Self::read_dir_files(&dir).and_then(|files| {
                manifest.check(k, n, &files)?;
                BodyIndicesFile::decode(k, &files[SegmentFile::BODY_INDICES], n)
            }) {
                Ok(bodies) => {
                    fs::write(dir.join(USED_MARKER), [])?;
                    return LoadedSegment::open(k, n, &dir, bodies, SegmentDir::Disk(lease));
                }
                Err(err) => {
                    tracing::warn!(segment = k, error = %err, "discarding corrupt disk cache entry");
                    if !self.discard(lease)? {
                        return self.load_ephemeral(&manifest).await;
                    }
                }
            }
        }
        let Some(reservation) = self.reserve(&root, manifest.total_bytes())? else {
            return self.load_ephemeral(&manifest).await;
        };
        let files = self.download(&manifest).await?;
        let bodies = files.body_indices()?;
        let fill = tempfile::Builder::new().prefix(FILL_PREFIX).tempdir_in(&root)?;
        Self::write(&files, fill.path())?;
        fs::write(fill.path().join(USED_MARKER), [])?;
        let lease = self.publish(k, fill, &dir)?;
        drop(reservation);
        LoadedSegment::open(k, n, &dir, bodies, SegmentDir::Disk(lease))
    }

    /// Downloads the objects `manifest` lists and checks them.
    pub async fn download(&self, manifest: &SegmentManifest) -> Result<SegmentFiles, HistoryStoreError> {
        let n = self.blocks_per_segment;
        self.downloads.fetch_add(1, Ordering::Relaxed);
        let files = self.objects.download(manifest, n).await?;
        manifest.check(manifest.segment, n, &files.files)?;
        Ok(files)
    }

    /// Downloads a segment into a temporary directory that is never persisted.
    pub async fn load_ephemeral(&self, manifest: &SegmentManifest) -> Result<LoadedSegment, HistoryStoreError> {
        let files = self.download(manifest).await?;
        let bodies = files.body_indices()?;
        let temp = tempfile::tempdir()?;
        Self::write(&files, temp.path())?;
        let path = temp.path().to_path_buf();
        LoadedSegment::open(manifest.segment, self.blocks_per_segment, &path, bodies, SegmentDir::Temp(temp))
    }

    /// Leases segment `k`'s published entry at `dir` if it exists. Checking and leasing under
    /// the disk lock means eviction cannot remove the entry in between.
    pub fn lease_existing(&self, k: u64, dir: &Path) -> Option<Arc<DiskLease>> {
        let mut disk = self.disk_state();
        dir.exists().then(|| disk.lease(k, dir))
    }

    /// Removes a corrupt entry unless another loaded segment still leases it. Returns whether
    /// the entry was removed.
    pub fn discard(&self, lease: Arc<DiskLease>) -> Result<bool, HistoryStoreError> {
        let mut disk = self.disk_state();
        if Arc::strong_count(&lease) > 1 {
            return Ok(false);
        }
        fs::remove_dir_all(&lease.dir)?;
        disk.leases.remove(&lease.segment);
        Ok(true)
    }

    /// Admits `bytes` to the disk tier, evicting unleased entries least recently used first.
    /// Returns `None`, evicting nothing, when the fill cannot fit beside leased entries and
    /// other fills' reservations.
    pub fn reserve(&self, root: &Path, bytes: u64) -> Result<Option<Reservation<'_>>, HistoryStoreError> {
        if bytes > self.config.disk_bytes {
            return Ok(None);
        }
        fs::create_dir_all(root)?;
        let mut disk = self.disk_state();
        let entries = Self::disk_entries(root)?;
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum::<u64>() + disk.reserved;
        let evictable: Vec<(u64, PathBuf)> = entries
            .into_iter()
            .filter(|(_, _, path)| Self::entry_segment(path).is_none_or(|k| disk.live_lease(k).is_none()))
            .map(|(_, size, path)| (size, path))
            .collect();
        let freeable: u64 = evictable.iter().map(|(size, _)| size).sum();
        if total - freeable + bytes > self.config.disk_bytes {
            return Ok(None);
        }
        for (size, path) in evictable {
            if total + bytes <= self.config.disk_bytes {
                break;
            }
            fs::remove_dir_all(&path)?;
            total -= size;
        }
        disk.reserved += bytes;
        Ok(Some(Reservation { cache: self, bytes }))
    }

    /// Renames a completed fill into place as segment `k`'s entry and leases it, under the disk
    /// lock so no eviction sees the entry unleased.
    pub fn publish(&self, k: u64, fill: TempDir, dir: &Path) -> Result<Arc<DiskLease>, HistoryStoreError> {
        let mut disk = self.disk_state();
        let fill = fill.keep();
        if let Err(err) = fs::rename(&fill, dir) {
            // Another process published the same entry first; its bytes are identical.
            fs::remove_dir_all(&fill)?;
            if !dir.exists() {
                return Err(err.into());
            }
        }
        Ok(disk.lease(k, dir))
    }

    /// Parses the segment number of a published entry directory.
    pub fn entry_segment(path: &Path) -> Option<u64> {
        path.file_name()?.to_str()?.strip_prefix(ENTRY_PREFIX)?.parse().ok()
    }

    /// Reads a disk entry back under archive object names.
    pub fn read_dir_files(dir: &Path) -> Result<BTreeMap<String, Bytes>, HistoryStoreError> {
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let object = if name == SegmentFile::BODY_INDICES {
                name
            } else if let Some(object) = SegmentFile::object_name(&name) {
                object
            } else {
                continue;
            };
            files.insert(object, Bytes::from(fs::read(entry.path())?));
        }
        Ok(files)
    }

    /// Writes a segment's static files and body index into `dir`.
    pub fn write(files: &SegmentFiles, dir: &Path) -> Result<(), HistoryStoreError> {
        files.write_static_files(dir)?;
        if let Some(raw) = files.files.get(SegmentFile::BODY_INDICES) {
            fs::write(dir.join(SegmentFile::BODY_INDICES), raw)?;
        }
        Ok(())
    }

    /// Published entries under `root` with their size, oldest first by last use.
    pub fn disk_entries(root: &Path) -> Result<Vec<(SystemTime, u64, PathBuf)>, HistoryStoreError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if Self::entry_segment(&path).is_none() || !path.is_dir() {
                continue;
            }
            let used = fs::metadata(path.join(USED_MARKER))
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let mut size = 0;
            for file in fs::read_dir(&path)? {
                size += file?.metadata()?.len();
            }
            entries.push((used, size, path));
        }
        entries.sort();
        Ok(entries)
    }

    /// Bytes currently held by the disk tier's published entries.
    pub fn disk_usage(&self) -> Result<u64, HistoryStoreError> {
        let Some(root) = self.disk_root().filter(|root| root.exists()) else { return Ok(0) };
        Ok(Self::disk_entries(&root)?.iter().map(|(_, size, _)| size).sum())
    }
}
