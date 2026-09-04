//! Progress-reporting helpers for snapshot uploads.

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
use humantime::{FormattedDuration, format_duration};
use tracing::{info, warn};

/// Interval between periodic progress logs during snapshot artifact uploads.
pub(crate) const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(3);

const UPLOAD_STALL_WARNING_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_STALLED_UPLOADS_IN_LOG: usize = 5;

/// Formats snapshot upload progress for structured logs.
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

    /// Formats a duration at whole-second precision for periodic progress logs.
    pub fn duration(duration: Duration) -> FormattedDuration {
        format_duration(Duration::from_secs(duration.as_secs()))
    }
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
                                    ProgressDisplay::duration(Duration::from_secs(state.idle_secs))
                                )
                            })
                            .collect();
                        warn!(
                            files = %format!("{files_done}/{total_files}"),
                            bytes = %ProgressDisplay::human_byte_progress(done, total_bytes),
                            stalled_for = %ProgressDisplay::duration(stalled_at.elapsed()),
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
                    elapsed = %ProgressDisplay::duration(started.elapsed()),
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
    use std::time::Duration;

    use super::ProgressDisplay;

    #[test]
    fn formats_byte_progress_with_binary_units() {
        assert_eq!(ProgressDisplay::human_byte_progress(1536, 2 * 1024 * 1024), "1.5 KiB/2 MiB");
    }

    #[test]
    fn formats_durations_at_whole_second_precision() {
        let duration = Duration::from_secs(5 * 60 + 3) + Duration::from_millis(241);
        assert_eq!(ProgressDisplay::duration(duration).to_string(), "5m 3s");
    }
}
