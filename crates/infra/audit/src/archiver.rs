use std::{
    fmt,
    marker::PhantomData,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tokio::{
    sync::{Mutex, mpsc},
    time::sleep,
};
use tracing::{error, info};

use crate::{
    metrics::Metrics,
    reader::{Event, EventReader},
    storage::EventWriter,
};

/// Archives audit events from Kafka to S3 storage.
pub struct KafkaAuditArchiver<R, W>
where
    R: EventReader,
    W: EventWriter + Clone + Send + 'static,
{
    reader: R,
    event_tx: mpsc::Sender<Event>,
    metrics: Metrics,
    _phantom: PhantomData<W>,
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
    pub fn new(
        reader: R,
        writer: W,
        worker_pool_size: usize,
        channel_buffer_size: usize,
        noop_archive: bool,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(channel_buffer_size);
        let metrics = Metrics::default();

        Self::spawn_workers(writer, event_rx, metrics.clone(), worker_pool_size, noop_archive);

        Self { reader, event_tx, metrics, _phantom: PhantomData }
    }

    fn spawn_workers(
        writer: W,
        event_rx: mpsc::Receiver<Event>,
        metrics: Metrics,
        worker_pool_size: usize,
        noop_archive: bool,
    ) {
        let event_rx = Arc::new(Mutex::new(event_rx));

        for worker_id in 0..worker_pool_size {
            let writer = writer.clone();
            let metrics = metrics.clone();
            let event_rx = Arc::clone(&event_rx);

            tokio::spawn(async move {
                loop {
                    let event = {
                        let mut rx = event_rx.lock().await;
                        rx.recv().await
                    };

                    match event {
                        Some(event) => {
                            let archive_start = Instant::now();
                            // tmp: only use this to clear kafka consumer offset
                            // TODO: use debug! later
                            if noop_archive {
                                info!(
                                    worker_id,
                                    bundle_id = %event.event.bundle_id(),
                                    tx_ids = ?event.event.transaction_ids(),
                                    timestamp = event.timestamp,
                                    "Noop archive - skipping event"
                                );
                                metrics.events_processed.increment(1);
                                metrics.in_flight_archive_tasks.decrement(1.0);
                                continue;
                            }
                            if let Err(e) = writer.archive_event(event).await {
                                error!(worker_id, error = %e, "Failed to write event");
                            } else {
                                metrics
                                    .archive_event_duration
                                    .record(archive_start.elapsed().as_secs_f64());
                                metrics.events_processed.increment(1);
                            }
                            metrics.in_flight_archive_tasks.decrement(1.0);
                        }
                        None => {
                            info!(worker_id, "Worker stopped - channel closed");
                            break;
                        }
                    }
                }
            });
        }
    }

    /// Runs the archiver loop, reading events and writing them to storage.
    pub async fn run(&mut self) -> Result<()> {
        loop {
            let read_start = Instant::now();
            match self.reader.read_event().await {
                Ok(event) => {
                    self.metrics.kafka_read_duration.record(read_start.elapsed().as_secs_f64());

                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let event_age_ms = now_ms.saturating_sub(event.timestamp);
                    self.metrics.event_age.record(event_age_ms as f64);

                    self.metrics.in_flight_archive_tasks.increment(1.0);
                    if let Err(e) = self.event_tx.send(event).await {
                        error!(error = %e, "Failed to send event to worker pool");
                        self.metrics.in_flight_archive_tasks.decrement(1.0);
                    }

                    let commit_start = Instant::now();
                    if let Err(e) = self.reader.commit().await {
                        error!(error = %e, "Failed to commit message");
                    }
                    self.metrics.kafka_commit_duration.record(commit_start.elapsed().as_secs_f64());
                }
                Err(e) => {
                    error!(error = %e, "Error reading events");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
