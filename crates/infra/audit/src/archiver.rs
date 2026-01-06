use crate::metrics::Metrics;
use crate::reader::EventReader;
use crate::storage::EventWriter;
use anyhow::Result;
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{error, info};

/// Archives audit events from Kafka to S3 storage.
pub struct KafkaAuditArchiver<R, W>
where
    R: EventReader,
    W: EventWriter + Clone + Send + 'static,
{
    reader: R,
    writer: W,
    metrics: Metrics,
}

impl<R, W> fmt::Debug for KafkaAuditArchiver<R, W>
where
    R: EventReader,
    W: EventWriter + Clone + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KafkaAuditArchiver").finish_non_exhaustive()
    }
}

impl<R, W> KafkaAuditArchiver<R, W>
where
    R: EventReader,
    W: EventWriter + Clone + Send + 'static,
{
    /// Creates a new archiver with the given reader and writer.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            metrics: Metrics::default(),
        }
    }

    /// Runs the archiver loop, reading events and writing them to storage.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting Kafka bundle archiver");

        loop {
            let read_start = Instant::now();
            match self.reader.read_event().await {
                Ok(event) => {
                    self.metrics
                        .kafka_read_duration
                        .record(read_start.elapsed().as_secs_f64());

                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let event_age_ms = now_ms.saturating_sub(event.timestamp);
                    self.metrics.event_age.record(event_age_ms as f64);

                    // TODO: the integration test breaks because Minio doesn't support etag
                    let writer = self.writer.clone();
                    let metrics = self.metrics.clone();
                    self.metrics.in_flight_archive_tasks.increment(1.0);
                    tokio::spawn(async move {
                        let archive_start = Instant::now();
                        if let Err(e) = writer.archive_event(event).await {
                            error!(error = %e, "Failed to write event");
                            metrics.failed_archive_tasks.increment(1);
                        } else {
                            metrics
                                .archive_event_duration
                                .record(archive_start.elapsed().as_secs_f64());
                            metrics.events_processed.increment(1);
                        }
                        metrics.in_flight_archive_tasks.decrement(1.0);
                    });

                    let commit_start = Instant::now();
                    if let Err(e) = self.reader.commit().await {
                        error!(error = %e, "Failed to commit message");
                    }
                    self.metrics
                        .kafka_commit_duration
                        .record(commit_start.elapsed().as_secs_f64());
                }
                Err(e) => {
                    error!(error = %e, "Error reading events");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
