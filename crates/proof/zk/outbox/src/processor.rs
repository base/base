use std::time::Duration;

use tokio::time;
use tracing::{debug, error, info};

use crate::{OutboxReader, OutboxTask, TaskQueue};

/// Orchestrates the outbox pattern: polls for tasks and submits them for processing.
pub struct OutboxProcessor<R: OutboxReader, Q: TaskQueue> {
    reader: R,
    queue: Q,
    poll_interval_secs: u64,
    batch_size: i64,
}

impl<R: OutboxReader, Q: TaskQueue> std::fmt::Debug for OutboxProcessor<R, Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxProcessor")
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("batch_size", &self.batch_size)
            .finish()
    }
}

impl<R: OutboxReader, Q: TaskQueue> OutboxProcessor<R, Q> {
    /// Create a new processor.
    pub const fn new(reader: R, queue: Q, poll_interval_secs: u64, batch_size: i64) -> Self {
        Self { reader, queue, poll_interval_secs, batch_size }
    }

    /// Run the processor loop indefinitely.
    pub async fn run(&self) {
        info!(
            poll_interval_secs = self.poll_interval_secs,
            batch_size = self.batch_size,
            "starting outbox processor"
        );

        let mut interval = time::interval(Duration::from_secs(self.poll_interval_secs));

        loop {
            interval.tick().await;

            if let Err(e) = self.process_batch().await {
                error!(error = %e, "failed to process outbox batch");
            }
        }
    }

    async fn process_batch(&self) -> anyhow::Result<()> {
        let tasks = self.reader.poll_tasks(self.batch_size).await?;

        if tasks.is_empty() {
            return Ok(());
        }

        debug!(count = tasks.len(), "polled outbox tasks");

        for task in tasks {
            self.process_task(task).await;
        }

        Ok(())
    }

    async fn process_task(&self, task: OutboxTask) {
        let sequence_id = task.sequence_id;
        let proof_request_id = task.proof_request_id;

        match self.queue.submit(task).await {
            Ok(()) => {
                // Task successfully submitted to queue — mark as processed in outbox.
                if let Err(e) = self.reader.mark_processed(sequence_id).await {
                    error!(
                        sequence_id = sequence_id,
                        error = %e,
                        "failed to mark task as processed in outbox"
                    );
                } else {
                    info!(
                        sequence_id = sequence_id,
                        proof_request_id = %proof_request_id,
                        "marked outbox entry as processed"
                    );
                }
            }
            Err(e) => {
                error!(
                    sequence_id = sequence_id,
                    proof_request_id = %proof_request_id,
                    error = %e,
                    "failed to submit task to queue"
                );
                if let Err(mark_err) = self.reader.mark_error(sequence_id, e.to_string()).await {
                    error!(
                        sequence_id = sequence_id,
                        error = %mark_err,
                        "failed to mark task as error"
                    );
                }
            }
        }
    }
}
