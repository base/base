//! The long-running archive job behind `archive-writer`: publish, retire, sweep, collect.
//!
//! Everything runs in one task so the writer and the pruner never race on the catalog CAS:
//! after each writer step the service checks whether maintenance is due and runs it before the
//! next step. The pruner and the orphan collector act under the writer's catalog epoch, so a
//! newer instance fences all three together.

use std::time::{Duration, Instant};

use chrono::Utc;
use tracing::{info, warn};

use crate::{
    ArchiveWriter, DEFAULT_MAX_SEGMENTS_PER_PASS, DEFAULT_ORPHAN_MIN_AGE, DEFAULT_RETIREMENT_GRACE,
    GcReport, HistoryStoreError, OrphanGc, Pruner, StepOutcome,
};

/// Retention and garbage-collection settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceConfig {
    /// Retire segments whose last block is older than this; `None` keeps everything.
    pub retention: Option<Duration>,
    /// Time between retirement and deletion of a segment's objects.
    pub retirement_grace: Duration,
    /// Minimum age of an orphaned object above `H` before it is deleted.
    pub orphan_min_age: Duration,
    /// How often maintenance runs.
    pub interval: Duration,
    /// Most segments retired, and most swept, per pass. Bounds one pass's object-store and
    /// index work so the writer is not starved after a long outage or a retention change.
    pub max_segments_per_pass: usize,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            retention: None,
            retirement_grace: DEFAULT_RETIREMENT_GRACE,
            orphan_min_age: DEFAULT_ORPHAN_MIN_AGE,
            interval: Duration::from_secs(600),
            max_segments_per_pass: DEFAULT_MAX_SEGMENTS_PER_PASS,
        }
    }
}

/// Outcome of one maintenance pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Segments retired.
    pub retired: Vec<u64>,
    /// Segments whose objects were deleted after the grace period.
    pub swept: Vec<u64>,
    /// Orphaned objects deleted.
    pub orphans_deleted: usize,
}

/// Writer plus retention jobs.
#[derive(Debug)]
pub struct ArchiveService {
    /// The writer.
    pub writer: ArchiveWriter,
    /// Retention job.
    pub pruner: Pruner,
    /// Orphan collector.
    pub gc: OrphanGc,
    /// Maintenance settings.
    pub maintenance: MaintenanceConfig,
}

impl ArchiveService {
    /// Builds the service around an opened writer, sharing its catalog and objects.
    pub fn new(writer: ArchiveWriter, maintenance: MaintenanceConfig) -> Self {
        let pruner = Pruner::new(
            writer.catalog.clone(),
            writer.objects.clone(),
            writer.epoch,
            maintenance.max_segments_per_pass,
        );
        let gc = OrphanGc::new(writer.catalog.clone(), writer.objects.clone(), writer.epoch);
        Self { writer, pruner, gc, maintenance }
    }

    /// Runs one maintenance pass: retire, sweep retired segments past the grace period, delete
    /// old orphans.
    pub async fn maintain(&self) -> Result<MaintenanceReport, HistoryStoreError> {
        let now = Utc::now();
        let now_ms = now.timestamp_millis().max(0) as u64;
        let retired = match self.maintenance.retention {
            Some(retention) => self.pruner.retire_older_than(&self.writer.index, retention, now_ms).await?,
            None => Vec::new(),
        };
        let swept = self.pruner.sweep(&self.writer.index, self.maintenance.retirement_grace, now_ms).await?;
        let orphans = self.collect_orphans().await?;
        Ok(MaintenanceReport { retired, swept, orphans_deleted: orphans.deleted.len() })
    }

    /// Deletes orphaned objects above `H` older than the configured minimum age.
    pub async fn collect_orphans(&self) -> Result<GcReport, HistoryStoreError> {
        self.gc.run(&self.writer.index, self.maintenance.orphan_min_age, Utc::now()).await
    }

    /// Runs until a terminal error. Source outages and catalog conflicts are logged and
    /// retried; anything else (fencing, corrupt data, misconfiguration) stops the service.
    pub async fn run(&self) -> Result<(), HistoryStoreError> {
        let mut last_maintenance: Option<Instant> = None;
        loop {
            let idle = match self.writer.step().await {
                Ok(StepOutcome::Published { segment, h, upload }) => {
                    info!(segment, h, created = upload.created, "published archive segment");
                    false
                }
                Ok(StepOutcome::Discarded { segment }) => {
                    info!(segment, "segment already published by another writer");
                    false
                }
                Ok(StepOutcome::NotReady { .. }) => true,
                Err(err @ (HistoryStoreError::NoSource { .. } | HistoryStoreError::Source(_))) => {
                    warn!(error = %err, "writer stalled; retrying");
                    true
                }
                Err(err) => return Err(err),
            };
            if last_maintenance.is_none_or(|at| at.elapsed() >= self.maintenance.interval) {
                match self.maintain().await {
                    Ok(report) if report != MaintenanceReport::default() => info!(
                        retired = ?report.retired,
                        swept = ?report.swept,
                        orphans_deleted = report.orphans_deleted,
                        "archive maintenance"
                    ),
                    Ok(_) => {}
                    Err(err @ HistoryStoreError::CasConflict) => {
                        warn!(error = %err, "maintenance lost a catalog race; retrying later");
                    }
                    Err(err) => return Err(err),
                }
                last_maintenance = Some(Instant::now());
            }
            if idle {
                tokio::time::sleep(self.writer.config.idle_interval).await;
            }
        }
    }
}
