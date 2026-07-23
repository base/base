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
use human_bytes::human_bytes as format_bytes;
use humantime::format_duration;
use tracing::{info, warn};

/// Interval between periodic progress logs during long-running snapshot operations
/// (archive compression and artifact upload).
pub(crate) const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(3);

const UPLOAD_STALL_WARNING_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_STALLED_UPLOADS_IN_LOG: usize = 5;

/// Formats snapshot compression and upload progress for structured logs.
#[derive(Debug)]
pub struct ProgressDisplay;

impl ProgressDisplay {
    /// Returns the integer completion percentage for a completed and total count.
    pub fn percent(done: u64, total: u64) -> u64 {
        done.saturating_mul(100).checked_div(total).unwrap_or(100)
    }

    /// Formats completed and total byte counts with human-readable binary units.
    pub fn human_byte_progress(done: u64, total: u64) -> String {
        format!("{}/{}", format_bytes(done as f64), format_bytes(total as f64))
    }
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
                bytes = %ProgressDisplay::human_byte_progress(self.bytes_done, self.total_bytes),
                progress = %format!("{}%", ProgressDisplay::percent(self.bytes_done, self.total_bytes)),
                elapsed = %format_duration(self.started.elapsed()),
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
    /// Registers an active archive within the component and returns a reporter
    /// that streams byte progress into that archive's row.
    pub(crate) fn start_archive(
        &self,
        archive_name: impl Into<String>,
        total_bytes: u64,
    ) -> ArchiveProgressReporter {
        let archive_name = archive_name.into();
        if let Ok(mut active_archives) = self.state.active_archives.lock() {
            active_archives
                .insert(archive_name.clone(), ActiveArchiveState { total_bytes, bytes_done: 0 });
        }
        ArchiveProgressReporter { component: self.clone(), archive_name }
    }

    /// Adds `n` compressed source bytes to the component-wide total.
    pub fn record(&self, n: u64) {
        self.state.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n` source bytes to one active archive and the component total.
    pub(crate) fn record_archive_bytes(&self, archive_name: &str, n: u64) {
        self.record(n);
        if let Ok(mut active_archives) = self.state.active_archives.lock()
            && let Some(state) = active_archives.get_mut(archive_name)
        {
            state.bytes_done = state.bytes_done.saturating_add(n).min(state.total_bytes);
        }
    }

    /// Marks one archive within the component as fully packaged.
    pub(crate) fn archive_completed(&self, archive_name: &str) {
        if let Ok(mut active_archives) = self.state.active_archives.lock()
            && let Some(state) = active_archives.remove(archive_name)
        {
            let remaining = state.total_bytes.saturating_sub(state.bytes_done);
            if remaining > 0 {
                self.record(remaining);
            }
        }
        self.state.archives_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Removes a failed archive from the active set without counting it complete.
    pub(crate) fn archive_failed(&self, archive_name: &str) {
        if let Ok(mut active_archives) = self.state.active_archives.lock() {
            active_archives.remove(archive_name);
        }
    }
}

/// Per-archive reporter backed by a shared component progress state.
#[derive(Clone, Debug)]
pub(crate) struct ArchiveProgressReporter {
    component: ComponentProgressReporter,
    archive_name: String,
}

impl ArchiveProgressReporter {
    /// Adds `n` source bytes to this archive and the parent component total.
    pub(crate) fn record(&self, n: u64) {
        self.component.record_archive_bytes(&self.archive_name, n);
    }

    /// Marks this archive as fully packaged and removes it from the active set.
    pub(crate) fn finish(&self) {
        self.component.archive_completed(&self.archive_name);
    }

    /// Removes this archive from the active set after a failure.
    pub(crate) fn fail(&self) {
        self.component.archive_failed(&self.archive_name);
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
            active_archives: Mutex::new(HashMap::new()),
        });
        let reporter = ComponentProgressReporter { state: Arc::clone(&state) };
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            while stop_rx.recv_timeout(PROGRESS_LOG_INTERVAL).is_err() {
                let bytes_done = state.bytes_done.load(Ordering::Relaxed);
                let archives_done = state.archives_done.load(Ordering::Relaxed);
                let active_archives = state.active_archives.lock().map_or(0, |active| active.len());
                info!(
                    component = %state.component_name,
                    archives = %format!("{archives_done}/{}", state.total_archives),
                    progress = %format!("{}%", ProgressDisplay::percent(bytes_done, state.total_bytes)),
                    bytes = %ProgressDisplay::human_byte_progress(bytes_done, state.total_bytes),
                    elapsed = %format_duration(state.started.elapsed()),
                    active_archives,
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
    /// Snapshot component name such as `headers` or `receipts`.
    pub component_name: String,
    /// Total uncompressed source bytes scheduled for this component.
    pub total_bytes: u64,
    /// Number of archives that will be produced for this component.
    pub total_archives: usize,
    /// Monotonic timestamp when component compression started.
    pub started: Instant,
    /// Aggregate uncompressed source bytes processed across all archives.
    pub bytes_done: AtomicU64,
    /// Number of archives that have fully completed compression.
    pub archives_done: AtomicU64,
    /// Currently active archives used for completion accounting and progress logs.
    pub active_archives: Mutex<HashMap<String, ActiveArchiveState>>,
}

/// Live progress state for one in-flight archive within a component.
#[derive(Debug)]
pub struct ActiveArchiveState {
    /// Total uncompressed source bytes for this archive.
    pub total_bytes: u64,
    /// Uncompressed source bytes processed so far for this archive.
    pub bytes_done: u64,
}

/// Cumulative upload progress shared across concurrent artifact uploads. A spawned
/// ticker reads the atomic byte counter and logs throughput once per interval.
#[derive(Debug)]
pub struct UploadProgress {
    uploaded: Arc<AtomicU64>,
    files_completed: Arc<AtomicU64>,
    total_bytes: u64,
    total_files: usize,
    active_uploads: Arc<Mutex<HashMap<String, UploadFileState>>>,
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

    /// Adds `n` uploaded bytes to the cumulative total and the named active file.
    pub(crate) fn add_for_file(&self, key: &str, n: u64) {
        self.add(n);
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.get_mut(key)
        {
            state.uploaded_bytes = state.uploaded_bytes.saturating_add(n).min(state.total_bytes);
            state.last_update = Instant::now();
            state.stage = UploadStage::Uploading;
        }
    }

    /// Registers an in-flight file upload for progress and stall tracking.
    pub(crate) fn start_file(&self, key: impl Into<String>, total_bytes: u64, stage: UploadStage) {
        let key = key.into();
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            let now = Instant::now();
            active_uploads.insert(
                key,
                UploadFileState { total_bytes, uploaded_bytes: 0, stage, last_update: now },
            );
        }
    }

    /// Updates the current stage for one active file upload.
    pub(crate) fn set_stage(&self, key: &str, stage: UploadStage) {
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.get_mut(key)
        {
            state.stage = stage;
            state.last_update = Instant::now();
        }
    }

    /// Removes a failed file upload from the active set.
    pub(crate) fn fail_file(&self, key: &str) {
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            active_uploads.remove(key);
        }
    }

    /// Marks one whole artifact as fully uploaded and removes it from the active set.
    pub(crate) fn finish_file(&self, key: &str) {
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.remove(key)
        {
            let remaining = state.total_bytes.saturating_sub(state.uploaded_bytes);
            if remaining > 0 {
                self.add(remaining);
            }
        }
        self.files_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Spawns the background progress logger.
    pub(crate) fn spawn_logger(&self) -> UploadProgressLogger {
        let uploaded = Arc::clone(&self.uploaded);
        let files_completed = Arc::clone(&self.files_completed);
        let total_bytes = self.total_bytes;
        let total_files = self.total_files;
        let active_uploads = Arc::clone(&self.active_uploads);
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut last_done = 0u64;
            let mut stalled_since: Option<Instant> = None;
            let mut last_stall_warning: Option<Instant> = None;

            while stop_rx.recv_timeout(PROGRESS_LOG_INTERVAL).is_err() {
                let done = uploaded.load(Ordering::Relaxed);
                let files_done = files_completed.load(Ordering::Relaxed);
                let active_snapshot = active_uploads.lock().ok().map(|active| {
                    let now = Instant::now();
                    let mut items: Vec<UploadFileSnapshot> = active
                        .iter()
                        .map(|(key, state)| UploadFileSnapshot {
                            key: key.clone(),
                            total_bytes: state.total_bytes,
                            uploaded_bytes: state.uploaded_bytes,
                            stage: state.stage,
                            idle_secs: now.duration_since(state.last_update).as_secs(),
                        })
                        .collect();
                    items.sort_unstable_by(|a, b| a.key.cmp(&b.key));
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
                            .map(|state| {
                                format!(
                                    "{} [{}] {} (idle {})",
                                    state.key,
                                    state.stage.label(),
                                    ProgressDisplay::human_byte_progress(
                                        state.uploaded_bytes,
                                        state.total_bytes
                                    ),
                                    format_duration(Duration::from_secs(state.idle_secs))
                                )
                            })
                            .collect();
                        warn!(
                            files = %format!("{files_done}/{total_files}"),
                            bytes = %ProgressDisplay::human_byte_progress(done, total_bytes),
                            stalled_for = %format_duration(stalled_at.elapsed()),
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
                    files = %format!("{files_done}/{total_files}"),
                    progress = %format!("{}%", ProgressDisplay::percent(done, total_bytes)),
                    bytes = %ProgressDisplay::human_byte_progress(done, total_bytes),
                    elapsed = %format_duration(started.elapsed()),
                    active_uploads = active_count,
                    "uploading snapshot artifacts (progress)"
                );
                last_done = done;
            }
        });

        UploadProgressLogger { stop_tx: Some(stop_tx), join_handle: Some(join_handle) }
    }
}

#[derive(Debug)]
pub(crate) struct UploadProgressLogger {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<StdJoinHandle<()>>,
}

impl UploadProgressLogger {
    /// Stops the background upload logger.
    pub(crate) fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for UploadProgressLogger {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum UploadStage {
    CreatingMultipart,
    Uploading,
    CompletingMultipart,
}

impl UploadStage {
    const fn label(self) -> &'static str {
        match self {
            Self::CreatingMultipart => "create",
            Self::Uploading => "upload",
            Self::CompletingMultipart => "complete",
        }
    }
}

#[derive(Debug)]
struct UploadFileState {
    total_bytes: u64,
    uploaded_bytes: u64,
    stage: UploadStage,
    last_update: Instant,
}

#[derive(Debug)]
struct UploadFileSnapshot {
    key: String,
    total_bytes: u64,
    uploaded_bytes: u64,
    stage: UploadStage,
    idle_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::ProgressDisplay;

    #[test]
    fn formats_byte_progress_with_binary_units() {
        assert_eq!(ProgressDisplay::human_byte_progress(1536, 2 * 1024 * 1024), "1.5 KiB/2 MiB");
    }
}
