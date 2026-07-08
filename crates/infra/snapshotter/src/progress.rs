//! Shared progress-reporting helpers used across snapshot generation and upload.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle as StdJoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Interval between periodic progress logs during long-running snapshot operations
/// (archive compression and artifact upload).
pub(crate) const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);

const UPLOAD_STALL_WARNING_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_STALLED_UPLOADS_IN_LOG: usize = 5;

const fn percent(done: u64, total: u64) -> u64 {
    if total == 0 { 100 } else { done.saturating_mul(100) / total }
}

/// Cumulative compression progress shared across every file in a single archive,
/// emitting a throttled log so large single-file archives report progress mid-stream.
#[derive(Debug)]
pub struct ArchiveProgress {
    archive_name: String,
    total_bytes: u64,
    started: Instant,
    last_log: Instant,
    bytes_done: u64,
}

impl ArchiveProgress {
    /// Creates a new tracker for an archive of `total_bytes` total uncompressed bytes.
    pub fn new(archive_name: String, total_bytes: u64) -> Self {
        let now = Instant::now();
        Self { archive_name, total_bytes, started: now, last_log: now, bytes_done: 0 }
    }

    /// Adds `n` newly-compressed bytes, emitting a progress log once per interval.
    pub fn record(&mut self, n: u64) {
        self.bytes_done += n;
        if self.last_log.elapsed() >= PROGRESS_LOG_INTERVAL {
            info!(
                archive = %self.archive_name,
                bytes_done = self.bytes_done,
                total_bytes = self.total_bytes,
                percent = percent(self.bytes_done, self.total_bytes),
                elapsed_secs = self.started.elapsed().as_secs(),
                "compressing archive"
            );
            self.last_log = Instant::now();
        }
    }
}

/// Shared compression progress for a whole chunked snapshot component such as
/// `transactions`, aggregating bytes across every archive compressed in parallel.
#[derive(Clone, Debug)]
pub struct ComponentProgressReporter {
    state: Arc<ComponentProgressState>,
}

impl ComponentProgressReporter {
    /// Adds `n` compressed source bytes to the component-wide total.
    pub fn record(&self, n: u64) {
        self.state.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    /// Marks one archive within the component as fully packaged.
    pub fn archive_completed(&self) {
        self.state.archives_done.fetch_add(1, Ordering::Relaxed);
    }
}

/// Owns a background logger that periodically reports one progress line for an
/// entire chunked component while its archives are being compressed.
pub struct ComponentProgressLogger {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<StdJoinHandle<()>>,
    reporter: ComponentProgressReporter,
}

impl std::fmt::Debug for ComponentProgressLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentProgressLogger")
            .field("reporter", &self.reporter)
            .finish_non_exhaustive()
    }
}

impl ComponentProgressLogger {
    /// Starts a periodic logger for one chunked component.
    pub fn new(component_name: String, total_bytes: u64, total_archives: usize) -> Self {
        let state = Arc::new(ComponentProgressState {
            component_name,
            total_bytes,
            total_archives,
            started: Instant::now(),
            bytes_done: AtomicU64::new(0),
            archives_done: AtomicU64::new(0),
        });
        let reporter = ComponentProgressReporter { state: Arc::clone(&state) };
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            while stop_rx.recv_timeout(PROGRESS_LOG_INTERVAL).is_err() {
                let bytes_done = state.bytes_done.load(Ordering::Relaxed);
                let archives_done = state.archives_done.load(Ordering::Relaxed);
                info!(
                    component = %state.component_name,
                    bytes_done,
                    total_bytes = state.total_bytes,
                    percent = percent(bytes_done, state.total_bytes),
                    archives_done,
                    total_archives = state.total_archives,
                    elapsed_secs = state.started.elapsed().as_secs(),
                    "compressing component"
                );
            }
        });
        Self { stop_tx: Some(stop_tx), join_handle: Some(join_handle), reporter }
    }

    /// Returns a cloneable reporter that worker threads can update concurrently.
    pub fn reporter(&self) -> ComponentProgressReporter {
        self.reporter.clone()
    }
}

impl Drop for ComponentProgressLogger {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Shared immutable metadata and atomics backing one component-wide compression logger.
#[derive(Debug)]
pub struct ComponentProgressState {
    component_name: String,
    total_bytes: u64,
    total_archives: usize,
    started: Instant,
    bytes_done: AtomicU64,
    archives_done: AtomicU64,
}

/// Cumulative upload progress shared across concurrent artifact uploads. A spawned
/// ticker reads the atomic byte counter and logs throughput once per interval.
#[derive(Debug)]
pub struct UploadProgress {
    uploaded: Arc<AtomicU64>,
    files_completed: Arc<AtomicU64>,
    total_bytes: u64,
    total_files: usize,
    active_uploads: Arc<Mutex<HashMap<String, Instant>>>,
}

impl UploadProgress {
    /// Builds a tracker whose total is the on-disk size of every file to be uploaded.
    /// Propagates any metadata error so the total stays consistent with the sizes the
    /// upload path itself records, preventing logged progress from overshooting 100%.
    pub async fn new(
        static_uploads: &[PathBuf],
        run_uploads: &[PathBuf],
        manifest_path: &Path,
    ) -> Result<Self> {
        let mut total_bytes = 0u64;
        let total_files = static_uploads.len() + run_uploads.len() + 1;
        let files = static_uploads
            .iter()
            .map(PathBuf::as_path)
            .chain(run_uploads.iter().map(PathBuf::as_path))
            .chain(std::iter::once(manifest_path));
        for file in files {
            let meta = tokio::fs::metadata(file)
                .await
                .with_context(|| format!("failed to stat {} for upload total", file.display()))?;
            total_bytes += meta.len();
        }
        Ok(Self {
            uploaded: Arc::new(AtomicU64::new(0)),
            files_completed: Arc::new(AtomicU64::new(0)),
            total_bytes,
            total_files,
            active_uploads: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Adds `n` successfully-uploaded bytes to the cumulative counter.
    pub fn add(&self, n: u64) {
        self.uploaded.fetch_add(n, Ordering::Relaxed);
    }

    /// Marks one whole artifact as fully uploaded.
    pub(crate) fn file_completed(&self) {
        self.files_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Registers an in-flight upload operation and removes it automatically when dropped.
    pub(crate) fn start_item(&self, label: impl Into<String>) -> UploadActivityGuard {
        let label = label.into();
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            active_uploads.insert(label.clone(), Instant::now());
        }
        UploadActivityGuard { label, active_uploads: Arc::clone(&self.active_uploads) }
    }

    /// Spawns a background task that logs upload progress once per interval until
    /// aborted via the returned handle.
    pub fn spawn_logger(&self) -> JoinHandle<()> {
        let uploaded = Arc::clone(&self.uploaded);
        let files_completed = Arc::clone(&self.files_completed);
        let total_bytes = self.total_bytes;
        let total_files = self.total_files;
        let active_uploads = Arc::clone(&self.active_uploads);
        tokio::spawn(async move {
            let started = Instant::now();
            let mut ticker = tokio::time::interval(PROGRESS_LOG_INTERVAL);
            let mut last_done = 0u64;
            let mut stalled_since: Option<Instant> = None;
            let mut last_stall_warning: Option<Instant> = None;
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let done = uploaded.load(Ordering::Relaxed);
                let files_done = files_completed.load(Ordering::Relaxed);
                let active_snapshot = active_uploads.lock().ok().map(|active| {
                    let now = Instant::now();
                    let mut items: Vec<(String, u64)> = active
                        .iter()
                        .map(|(label, started)| {
                            (label.clone(), now.duration_since(*started).as_secs())
                        })
                        .collect();
                    items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
                    items
                });
                let active_count = active_snapshot.as_ref().map_or(0, Vec::len);

                if done == last_done && active_count > 0 {
                    let stalled_at = stalled_since.get_or_insert_with(Instant::now);
                    let should_warn = stalled_at.elapsed() >= UPLOAD_STALL_WARNING_AFTER
                        && last_stall_warning.is_none_or(|last_warning| {
                            last_warning.elapsed() >= UPLOAD_STALL_WARNING_AFTER
                        });
                    if should_warn {
                        let stalled_uploads: Vec<String> = active_snapshot
                            .as_ref()
                            .into_iter()
                            .flatten()
                            .take(MAX_STALLED_UPLOADS_IN_LOG)
                            .map(|(label, age_secs)| format!("{label} ({age_secs}s)"))
                            .collect();
                        warn!(
                            bytes_uploaded = done,
                            total_bytes,
                            files_uploaded = files_done,
                            total_files,
                            stalled_secs = stalled_at.elapsed().as_secs(),
                            active_uploads = active_count,
                            stalled_uploads = ?stalled_uploads,
                            "upload progress is stalled"
                        );
                        last_stall_warning = Some(Instant::now());
                    }
                } else {
                    stalled_since = None;
                    last_stall_warning = None;
                }

                info!(
                    bytes_uploaded = done,
                    total_bytes,
                    percent = percent(done, total_bytes),
                    files_uploaded = files_done,
                    total_files,
                    active_uploads = active_count,
                    elapsed_secs = started.elapsed().as_secs(),
                    "uploading snapshot artifacts (progress)"
                );
                last_done = done;
            }
        })
    }
}

#[derive(Debug)]
pub(crate) struct UploadActivityGuard {
    label: String,
    active_uploads: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Drop for UploadActivityGuard {
    fn drop(&mut self) {
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            active_uploads.remove(&self.label);
        }
    }
}
