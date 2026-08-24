//! Durable recording of accepted node reports.

use std::{
    fmt,
    fs::{OpenOptions, create_dir_all},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base_telemetry_types::NodeReportEvent;
use tracing::{info, warn};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};

/// Bounded queue depth in front of the JSONL file.
///
/// A report is roughly a kilobyte and arrives once per node per reporting interval, so this
/// absorbs a large burst. Past it the recorder drops rather than blocking the handler.
pub const DEFAULT_RECORDER_QUEUE_CAPACITY: usize = 8192;

/// Accepts node reports that passed validation.
///
/// This is the fan-out seam. v1 has one implementation; Datadog and S3 land behind it without
/// touching the ingest handler.
pub trait ReportRecorder: fmt::Debug + Send + Sync + 'static {
    /// Records one accepted report. Must not block and must not fail the request.
    fn record(&self, event: &NodeReportEvent);
}

/// Records reports as a structured log event and, when a path is configured, as JSONL.
///
/// The file is opened in append mode and written through a background worker, so a slow or
/// full disk cannot stall the ingest handler. Rotation is left to the operator's log rotation:
/// the worker reopens nothing, so configure `copytruncate` or restart the service after a
/// rotation.
pub struct JsonlRecorder {
    inner: Arc<RecorderInner>,
}

struct RecorderInner {
    writer: Option<NonBlocking>,
    errors: Option<ErrorCounter>,
    reported_drops: AtomicU64,
    path: Option<PathBuf>,
    _guard: Option<WorkerGuard>,
}

impl fmt::Debug for JsonlRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonlRecorder")
            .field("path", &self.inner.path)
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl JsonlRecorder {
    /// Opens `path` for append and starts the background writer.
    ///
    /// Parent directories are created if missing.
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let (writer, guard) = NonBlockingBuilder::default()
            .lossy(true)
            .buffered_lines_limit(DEFAULT_RECORDER_QUEUE_CAPACITY)
            .finish(file);
        let errors = writer.error_counter();

        info!(path = %path.display(), "recording node reports to JSONL");

        Ok(Self {
            inner: Arc::new(RecorderInner {
                writer: Some(writer),
                errors: Some(errors),
                reported_drops: AtomicU64::new(0),
                path: Some(path),
                _guard: Some(guard),
            }),
        })
    }

    /// Returns a recorder that only emits the structured log event.
    pub fn log_only() -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                writer: None,
                errors: None,
                reported_drops: AtomicU64::new(0),
                path: None,
                _guard: None,
            }),
        }
    }

    /// Returns the number of reports the background writer has dropped.
    pub fn dropped(&self) -> u64 {
        self.inner.errors.as_ref().map_or(0, |errors| errors.dropped_lines() as u64)
    }

    /// Returns the JSONL path, if one is configured.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// Logs newly observed drops exactly once each, so a full queue does not log per report.
    fn report_new_drops(&self) {
        let dropped = self.dropped();
        let previous = self.inner.reported_drops.swap(dropped, Ordering::Relaxed);
        if dropped > previous {
            warn!(
                dropped_total = dropped,
                newly_dropped = dropped - previous,
                "node report writer queue full, reports dropped"
            );
        }
    }
}

impl Clone for JsonlRecorder {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl ReportRecorder for JsonlRecorder {
    fn record(&self, event: &NodeReportEvent) {
        info!(
            telemetry_id = %event.report.telemetry_id,
            schema_version = %event.report.schema_version,
            version = %event.report.client.client_version,
            git_sha = %event.report.client.git_sha,
            l2_chain_id = event.report.client.l2_chain_id,
            network = %event.report.client.network,
            layer = event.report.client.layer.as_str(),
            role = event.report.client.role.as_str(),
            unsafe_block = event.report.heads.unsafe_block,
            unsafe_latency_secs = event.report.heads.unsafe_latency_secs,
            worst_unsafe_latency_secs = event.report.heads.worst_unsafe_latency_secs,
            peer_count = event.report.net_health.peer_count,
            hardware_platform = event.report.hardware.platform.as_str(),
            ip_source = event.ip_source.as_str(),
            "node report accepted"
        );

        let Some(writer) = self.inner.writer.as_ref() else {
            return;
        };
        let Ok(mut line) = serde_json::to_vec(event) else {
            warn!(telemetry_id = %event.report.telemetry_id, "failed to serialize node report");
            return;
        };
        line.push(b'\n');

        let mut writer = writer.clone();
        if let Err(error) = writer.write_all(&line) {
            warn!(error = %error, "failed to append node report to JSONL");
        }
        self.report_new_drops();
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use base_telemetry_types::{Heads, IpSource, NodeReport, NodeReportEvent};
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;

    fn event() -> NodeReportEvent {
        let report = NodeReport {
            telemetry_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            heads: Heads { unsafe_block: 42, ..Default::default() },
            ..Default::default()
        };
        NodeReportEvent::new(report, Utc::now(), IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)))
    }

    #[test]
    fn test_records_one_jsonl_line_per_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("node-reports.jsonl");
        let recorder = JsonlRecorder::new(&path).unwrap();

        recorder.record(&event());
        recorder.record(&event());
        drop(recorder);

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let decoded: NodeReportEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(decoded.report.heads.unsafe_block, 42);
        assert_eq!(decoded.ip_source, IpSource::ServerObserved);
        assert_eq!(decoded.reported_ip, IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    }

    #[test]
    fn test_log_only_recorder_writes_no_file() {
        let recorder = JsonlRecorder::log_only();

        recorder.record(&event());

        assert!(recorder.path().is_none());
        assert_eq!(recorder.dropped(), 0);
    }

    #[test]
    fn test_opening_an_unwritable_path_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-reports.jsonl");
        std::fs::create_dir(&path).unwrap();

        assert!(JsonlRecorder::new(&path).is_err());
    }
}
