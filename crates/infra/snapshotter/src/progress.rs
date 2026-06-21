//! Shared progress-reporting helpers used across snapshot generation and upload.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::info;

/// Interval between periodic progress logs during long-running snapshot operations
/// (archive compression and artifact upload).
pub(crate) const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);

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

/// Cumulative upload progress shared across concurrent artifact uploads. A spawned
/// ticker reads the atomic byte counter and logs throughput once per interval.
#[derive(Debug)]
pub struct UploadProgress {
    uploaded: Arc<AtomicU64>,
    total_bytes: u64,
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
        Ok(Self { uploaded: Arc::new(AtomicU64::new(0)), total_bytes })
    }

    /// Adds `n` successfully-uploaded bytes to the cumulative counter.
    pub fn add(&self, n: u64) {
        self.uploaded.fetch_add(n, Ordering::Relaxed);
    }

    /// Spawns a background task that logs upload progress once per interval until
    /// aborted via the returned handle.
    pub fn spawn_logger(&self) -> JoinHandle<()> {
        let uploaded = Arc::clone(&self.uploaded);
        let total_bytes = self.total_bytes;
        tokio::spawn(async move {
            let started = Instant::now();
            let mut ticker = tokio::time::interval(PROGRESS_LOG_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let done = uploaded.load(Ordering::Relaxed);
                info!(
                    bytes_uploaded = done,
                    total_bytes,
                    percent = percent(done, total_bytes),
                    elapsed_secs = started.elapsed().as_secs(),
                    "uploading snapshot artifacts (progress)"
                );
            }
        })
    }
}
