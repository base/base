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
    collections::HashMap,
    fs, io,
    num::NonZeroUsize,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use base_common_consensus::BasePrimitives;
use lru::LruCache;
use reth_provider::providers::{StaticFileProvider, StaticFileProviderBuilder};
use roaring::RoaringBitmap;
use tempfile::TempDir;

use crate::{
    ArchiveReader, BodyIndicesFile, Catalog, HistoryStoreError, IndexReader, SegmentManifest,
    SegmentObjects,
};

/// File touched on every disk-tier hit; its mtime orders LRU eviction.
const USED_MARKER: &str = "USED";

/// Prefix of in-progress fill directories, renamed into place once complete.
const FILL_PREFIX: &str = "fill-";

/// Directory under the cache directory holding segments served without persisting them.
const EPHEMERAL_DIR: &str = "ephemeral";

/// Prefix of the directories in [`EPHEMERAL_DIR`], one per ephemerally loaded segment.
const EPHEMERAL_PREFIX: &str = "ephemeral-";

/// Prefix of published disk entries, followed by the segment number.
const ENTRY_PREFIX: &str = "seg-";

/// Default number of segment loads that may run at once
/// ([`CacheConfig::max_concurrent_fills`]).
pub const DEFAULT_MAX_CONCURRENT_FILLS: usize = 2;

/// Cache sizing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    /// Segments kept open in memory.
    pub memory_segments: NonZeroUsize,
    /// Optional disk cache directory (`--archive.cache-dir`); off by default.
    pub disk_dir: Option<PathBuf>,
    /// Byte budget of the disk cache (`--archive.cache-size`).
    pub disk_bytes: u64,
    /// Segment loads (downloads and disk-entry checks) that may run at once across all
    /// segments. Each load streams a whole segment through SHA-256, so this bounds the IO and
    /// hashing that cold reads can start together.
    pub max_concurrent_fills: NonZeroUsize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_segments: NonZeroUsize::new(4).expect("non-zero"),
            disk_dir: None,
            disk_bytes: 1 << 30,
            max_concurrent_fills: NonZeroUsize::new(DEFAULT_MAX_CONCURRENT_FILLS)
                .expect("non-zero"),
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
    /// Blocks whose CPU-heavy checks already passed while this segment is loaded.
    pub verified: SegmentVerification,
}

/// Blocks of a loaded segment whose transaction root and senders, or receipt root, were already
/// checked. Sender recovery and receipt-root hashing dominate archive read CPU, so each runs at
/// most once per block while the segment stays loaded instead of once per request. Checking a
/// whole segment at load time would make the first read of a segment pay for every block in it.
#[derive(Debug, Default)]
pub struct SegmentVerification {
    /// Block offsets whose transaction root and senders were checked.
    pub bodies: Mutex<RoaringBitmap>,
    /// Block offsets whose receipt root was checked.
    pub receipts: Mutex<RoaringBitmap>,
    /// Checks run so far, body and receipt checks counted separately.
    pub checks: AtomicU64,
}

impl SegmentVerification {
    /// Whether `offset` is in `set`.
    pub fn contains(set: &Mutex<RoaringBitmap>, offset: u32) -> bool {
        set.lock().unwrap_or_else(PoisonError::into_inner).contains(offset)
    }

    /// Records a passed check of `offset` in `set`.
    pub fn record(&self, set: &Mutex<RoaringBitmap>, offset: u32) {
        set.lock().unwrap_or_else(PoisonError::into_inner).insert(offset);
        self.checks.fetch_add(1, Ordering::Relaxed);
    }
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
        Ok(Self {
            segment,
            first_block: segment * blocks_per_segment,
            provider,
            bodies,
            dir,
            verified: SegmentVerification::default(),
        })
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
pub struct Reservation {
    /// Disk tier the bytes were admitted to.
    pub tier: Arc<DiskTier>,
    /// Admitted bytes.
    pub bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.tier.state().reserved -= self.bytes;
    }
}

/// Outcome of looking for a segment's published disk entry.
#[derive(Debug)]
pub enum ExistingEntry {
    /// The entry passed its manifest check and is open.
    Hit(LoadedSegment),
    /// No entry, or a corrupt one that was removed: fill it from the bucket.
    Absent,
    /// A corrupt entry another loaded segment still leases: serve this read ephemerally.
    Leased,
}

/// The on-disk tier under `<disk_dir>/<namespace>`. Every method does blocking filesystem IO,
/// so async callers run them through [`ArchiveReader::blocking`].
#[derive(Debug)]
pub struct DiskTier {
    /// Directory holding this archive's entries.
    pub root: PathBuf,
    /// Directory holding segments served without persisting them (`<disk_dir>/ephemeral`).
    /// It lies outside [`Self::root`], so its bytes never count against [`Self::budget`].
    pub ephemeral: PathBuf,
    /// Byte budget of published entries plus reservations.
    pub budget: u64,
    /// Leases and reservations.
    pub state: Mutex<DiskState>,
}

impl DiskTier {
    /// Locks the disk-tier state.
    pub fn state(&self) -> std::sync::MutexGuard<'_, DiskState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Directory of segment `k`'s published entry.
    pub fn entry_dir(&self, k: u64) -> PathBuf {
        self.root.join(format!("{ENTRY_PREFIX}{k}"))
    }

    /// Brings a persisted disk tier back within the budget before any lease exists.
    ///
    /// A `fill-*` directory left by a crashed process is never valid (publication is an atomic
    /// rename), and its bytes are not covered by any reservation, so it is removed. Entries
    /// written under a larger budget are then evicted least recently used first until the
    /// published entries fit; an entry larger than the whole budget is removed, so later reads
    /// of that segment follow the ephemeral policy instead of leasing it in place.
    pub fn reconcile(&self) -> Result<(), HistoryStoreError> {
        // A directory under `ephemeral/` belongs to a segment loaded by a process that is gone:
        // a live one removes its own directories when the segment is dropped.
        if self.ephemeral.exists() {
            for entry in fs::read_dir(&self.ephemeral)? {
                let path = entry?.path();
                let stale = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(EPHEMERAL_PREFIX));
                if stale {
                    tracing::info!(path = %path.display(), "removing stale ephemeral segment");
                    fs::remove_dir_all(&path)?;
                }
            }
        }
        if !self.root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.root)? {
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
        let entries = Self::disk_entries(&self.root)?;
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        // Once the total fits, every remaining entry fits too, including against a budget
        // smaller than one entry, which this loop empties.
        for (_, size, path) in entries {
            if total <= self.budget {
                break;
            }
            tracing::info!(path = %path.display(), size, "evicting cache entry over the budget");
            fs::remove_dir_all(&path)?;
            total -= size;
        }
        Ok(())
    }

    /// Opens segment `k`'s published entry if it exists and matches `manifest`. Files are
    /// hashed as they are read ([`SegmentManifest::check_dir`]), never loaded whole. A corrupt
    /// entry is removed unless another loaded segment still leases it.
    pub fn open_existing(
        &self,
        k: u64,
        n: u64,
        manifest: &SegmentManifest,
    ) -> Result<ExistingEntry, HistoryStoreError> {
        let Some(lease) = self.lease_existing(k) else { return Ok(ExistingEntry::Absent) };
        let dir = lease.dir.clone();
        match manifest.check_dir(k, n, &dir).and_then(|()| BodyIndicesFile::read(k, &dir, n)) {
            Ok(bodies) => {
                fs::write(dir.join(USED_MARKER), [])?;
                let loaded = LoadedSegment::open(k, n, &dir, bodies, SegmentDir::Disk(lease))?;
                Ok(ExistingEntry::Hit(loaded))
            }
            Err(err) => {
                tracing::warn!(segment = k, error = %err, "discarding corrupt disk cache entry");
                if self.discard(lease)? {
                    Ok(ExistingEntry::Absent)
                } else {
                    Ok(ExistingEntry::Leased)
                }
            }
        }
    }

    /// Leases segment `k`'s published entry if it exists. Checking and leasing under the disk
    /// lock means eviction cannot remove the entry in between.
    pub fn lease_existing(&self, k: u64) -> Option<Arc<DiskLease>> {
        let dir = self.entry_dir(k);
        let mut disk = self.state();
        dir.exists().then(|| disk.lease(k, &dir))
    }

    /// Removes a corrupt entry unless another loaded segment still leases it. Returns whether
    /// the entry was removed.
    pub fn discard(&self, lease: Arc<DiskLease>) -> Result<bool, HistoryStoreError> {
        let mut disk = self.state();
        if Arc::strong_count(&lease) > 1 {
            return Ok(false);
        }
        fs::remove_dir_all(&lease.dir)?;
        disk.leases.remove(&lease.segment);
        Ok(true)
    }

    /// Admits `bytes`, evicting unleased entries least recently used first. Returns `None`,
    /// evicting nothing, when the fill cannot fit beside leased entries and other fills'
    /// reservations.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Result<Option<Reservation>, HistoryStoreError> {
        if bytes > self.budget {
            return Ok(None);
        }
        fs::create_dir_all(&self.root)?;
        let mut disk = self.state();
        let entries = Self::disk_entries(&self.root)?;
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum::<u64>() + disk.reserved;
        let evictable: Vec<(u64, PathBuf)> = entries
            .into_iter()
            .filter(|(_, _, path)| {
                Self::entry_segment(path).is_none_or(|k| disk.live_lease(k).is_none())
            })
            .map(|(_, size, path)| (size, path))
            .collect();
        let freeable: u64 = evictable.iter().map(|(size, _)| size).sum();
        if total - freeable + bytes > self.budget {
            return Ok(None);
        }
        for (size, path) in evictable {
            if total + bytes <= self.budget {
                break;
            }
            fs::remove_dir_all(&path)?;
            total -= size;
        }
        disk.reserved += bytes;
        Ok(Some(Reservation { tier: Arc::clone(self), bytes }))
    }

    /// Creates an empty `ephemeral-*` directory for a segment served without persisting it.
    pub fn ephemeral_dir(&self) -> Result<TempDir, HistoryStoreError> {
        fs::create_dir_all(&self.ephemeral)?;
        Ok(tempfile::Builder::new().prefix(EPHEMERAL_PREFIX).tempdir_in(&self.ephemeral)?)
    }

    /// Creates an empty `fill-*` directory for a new entry.
    pub fn fill_dir(&self) -> Result<TempDir, HistoryStoreError> {
        Ok(tempfile::Builder::new().prefix(FILL_PREFIX).tempdir_in(&self.root)?)
    }

    /// Publishes a completed, manifest-checked fill as segment `k`'s entry and opens it.
    pub fn publish_fill(
        &self,
        k: u64,
        n: u64,
        fill: TempDir,
    ) -> Result<LoadedSegment, HistoryStoreError> {
        let bodies = BodyIndicesFile::read(k, fill.path(), n)?;
        fs::write(fill.path().join(USED_MARKER), [])?;
        let lease = self.publish(k, fill)?;
        let dir = lease.dir.clone();
        LoadedSegment::open(k, n, &dir, bodies, SegmentDir::Disk(lease))
    }

    /// Renames a completed fill into place as segment `k`'s entry and leases it, under the disk
    /// lock so no eviction sees the entry unleased.
    pub fn publish(&self, k: u64, fill: TempDir) -> Result<Arc<DiskLease>, HistoryStoreError> {
        let dir = self.entry_dir(k);
        let mut disk = self.state();
        let fill = fill.keep();
        if let Err(err) = fs::rename(&fill, &dir) {
            // Another process published the same entry first; its bytes are identical.
            fs::remove_dir_all(&fill)?;
            if !dir.exists() {
                return Err(err.into());
            }
        }
        Ok(disk.lease(k, &dir))
    }

    /// Parses the segment number of a published entry directory.
    pub fn entry_segment(path: &Path) -> Option<u64> {
        path.file_name()?.to_str()?.strip_prefix(ENTRY_PREFIX)?.parse().ok()
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

    /// Bytes held by published entries.
    pub fn usage(&self) -> Result<u64, HistoryStoreError> {
        if !self.root.exists() {
            return Ok(0);
        }
        Ok(Self::disk_entries(&self.root)?.iter().map(|(_, size, _)| size).sum())
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
/// - **Bounded memory.** Objects stream from the bucket to their file chunk by chunk, hashed as
///   they arrive, and disk entries are hashed as they are read, so a fill holds one chunk per
///   object, never a whole segment. All filesystem IO, hashing and provider opens run on the
///   blocking pool, and at most [`CacheConfig::max_concurrent_fills`] loads run at once across
///   segments; a read waiting for a load slot can be cancelled without leaving state behind.
/// - **Namespacing.** Disk entries live under `<disk_dir>/<namespace>/seg-<k>`, where the
///   namespace is the archive identity (chain id, genesis hash, format, `N`). Archives with
///   another identity never share entries. Within one identity, segment bytes are
///   deterministic and manifest-checked, so two buckets of the same chain may share entries.
/// - **Single flight.** Concurrent misses for one segment wait on one fill. The fill writes
///   into a `fill-*` directory and renames it into place, so readers and other processes never
///   see a partial entry. A fill that fails its checks removes its directory.
/// - **Leases.** Every segment loaded from the disk tier leases its entry, and eviction skips
///   leased entries, because reth opens jars lazily by path after the segment is loaded.
///   Segments held by the memory tier stay leased, so the budget must exceed
///   `memory_segments` segments for the disk tier to hold anything else.
/// - **Budget.** A fill reserves its segment's manifest size before downloading, under the
///   same lock as eviction, and keeps the reservation until its entry is published. Published
///   entries plus reservations never exceed `disk_bytes`; a fill that cannot be admitted
///   (larger than the budget, or blocked by leased entries and other fills) is served from an
///   `ephemeral-*` directory under `<disk_dir>/ephemeral` and never persisted. Ephemeral
///   segments live only while loaded, so they add at most `memory_segments +
///   max_concurrent_fills` segments of disk beyond `disk_bytes`. Leases and reservations are process-local: two
///   processes sharing one cache directory each enforce the budget for their own fills only.
/// - **Restart.** Opening the cache removes abandoned `fill-*` and `ephemeral-*` directories and
///   evicts persisted entries until they fit the current budget ([`DiskTier::reconcile`]),
///   before any lease.
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
    /// Disk tier, if configured.
    pub disk: Option<Arc<DiskTier>>,
    /// Load slots shared by every segment ([`CacheConfig::max_concurrent_fills`]).
    pub fill_permits: tokio::sync::Semaphore,
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
    /// Creates a cache for the archive described by `catalog`, reconciling a persisted disk
    /// tier with the configured budget first.
    pub async fn new(
        config: CacheConfig,
        objects: SegmentObjects,
        catalog: &Catalog,
    ) -> Result<Self, HistoryStoreError> {
        let blocks_per_segment = catalog.blocks_per_segment()?;
        let namespace = format!(
            "{}-{}-f{}-n{blocks_per_segment}",
            catalog.chain_id, catalog.genesis_hash, catalog.format
        );
        let disk = config.disk_dir.as_ref().map(|dir| {
            Arc::new(DiskTier {
                root: dir.join(&namespace),
                ephemeral: dir.join(EPHEMERAL_DIR),
                budget: config.disk_bytes,
                state: Mutex::default(),
            })
        });
        if let Some(tier) = &disk {
            let tier = Arc::clone(tier);
            ArchiveReader::blocking(move || tier.reconcile()).await?;
        }
        let memory = Mutex::new(LruCache::new(config.memory_segments));
        let fill_permits = tokio::sync::Semaphore::new(config.max_concurrent_fills.get());
        Ok(Self {
            config,
            objects,
            blocks_per_segment,
            namespace,
            memory,
            fills: Mutex::default(),
            disk,
            fill_permits,
            downloads: AtomicU64::new(0),
        })
    }

    /// Number of segments in the memory tier.
    pub fn memory_len(&self) -> usize {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Directory holding this archive's disk entries, if the disk tier is on.
    pub fn disk_root(&self) -> Option<PathBuf> {
        self.disk.as_ref().map(|tier| tier.root.clone())
    }

    /// Bytes admitted for fills that have not published yet.
    pub fn disk_reserved(&self) -> u64 {
        self.disk.as_ref().map_or(0, |tier| tier.state().reserved)
    }

    /// Bytes currently held by the disk tier's published entries. Does blocking IO.
    pub fn disk_usage(&self) -> Result<u64, HistoryStoreError> {
        self.disk.as_ref().map_or(Ok(0), |tier| tier.usage())
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
        let _permit = self.fill_permits.acquire().await.map_err(io::Error::other)?;
        let loaded = Arc::new(self.load(k, index).await?);
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).put(k, Arc::clone(&loaded));
        Ok(loaded)
    }

    /// Returns segment `k` from the memory tier.
    pub fn memory_hit(&self, k: u64) -> Option<Arc<LoadedSegment>> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).get(&k).map(Arc::clone)
    }

    /// Loads segment `k` from the disk tier or the bucket, verified against its manifest.
    pub async fn load(
        &self,
        k: u64,
        index: &IndexReader,
    ) -> Result<LoadedSegment, HistoryStoreError> {
        let n = self.blocks_per_segment;
        let manifest = index.manifest(k).await?;
        let Some(tier) = self.disk.clone() else { return self.load_ephemeral(&manifest).await };
        let existing = {
            let (tier, manifest) = (Arc::clone(&tier), manifest.clone());
            ArchiveReader::blocking(move || tier.open_existing(k, n, &manifest)).await?
        };
        match existing {
            ExistingEntry::Hit(loaded) => return Ok(loaded),
            ExistingEntry::Leased => return self.load_ephemeral(&manifest).await,
            ExistingEntry::Absent => {}
        }
        let reservation = {
            let (tier, bytes) = (Arc::clone(&tier), manifest.total_bytes());
            ArchiveReader::blocking(move || tier.reserve(bytes)).await?
        };
        let Some(reservation) = reservation else {
            return self.load_ephemeral(&manifest).await;
        };
        let fill = {
            let tier = Arc::clone(&tier);
            ArchiveReader::blocking(move || tier.fill_dir()).await?
        };
        if let Err(err) = self.download_to(&manifest, fill.path()).await {
            return Self::abandon(fill, err).await;
        }
        let loaded = ArchiveReader::blocking(move || tier.publish_fill(k, n, fill)).await?;
        drop(reservation);
        Ok(loaded)
    }

    /// Streams the objects `manifest` lists into `dir`, checking each against the manifest.
    pub async fn download_to(
        &self,
        manifest: &SegmentManifest,
        dir: &Path,
    ) -> Result<(), HistoryStoreError> {
        self.downloads.fetch_add(1, Ordering::Relaxed);
        self.objects.download_to(manifest, self.blocks_per_segment, dir).await
    }

    /// Removes a failed fill's directory on the blocking pool and returns the fill's error.
    pub async fn abandon<T>(dir: TempDir, err: HistoryStoreError) -> Result<T, HistoryStoreError> {
        let path = dir.path().to_path_buf();
        if let Err(close) = ArchiveReader::blocking(move || Ok(dir.close()?)).await {
            tracing::warn!(path = %path.display(), error = %close, "failed to remove cache fill");
        }
        Err(err)
    }

    /// Downloads a segment into a temporary directory that is never persisted: under the
    /// cache directory's `ephemeral/` when the disk tier is on, so ephemeral segments never
    /// land in the system temp dir (often RAM-backed) of a node, else the system temp dir.
    pub async fn load_ephemeral(
        &self,
        manifest: &SegmentManifest,
    ) -> Result<LoadedSegment, HistoryStoreError> {
        let (k, n) = (manifest.segment, self.blocks_per_segment);
        let tier = self.disk.clone();
        let temp = ArchiveReader::blocking(move || match tier {
            Some(tier) => tier.ephemeral_dir(),
            None => Ok(tempfile::tempdir()?),
        })
        .await?;
        if let Err(err) = self.download_to(manifest, temp.path()).await {
            return Self::abandon(temp, err).await;
        }
        ArchiveReader::blocking(move || {
            let path = temp.path().to_path_buf();
            let bodies = BodyIndicesFile::read(k, &path, n)?;
            LoadedSegment::open(k, n, &path, bodies, SegmentDir::Temp(temp))
        })
        .await
    }
}
