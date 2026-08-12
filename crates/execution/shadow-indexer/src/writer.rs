use std::time::Duration;

use async_trait::async_trait;
use base_shadow_indexer_db::{ShadowBlockRepo, ShadowBlockRow, ShadowDbConfig};
use reth_tasks::TaskExecutor;
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, sleep},
};
use tracing::{error, info};

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_FLUSH_ATTEMPTS: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

#[cfg_attr(test, mockall::automock)]
#[async_trait]
trait BlockInserter: Send + Sync {
    async fn insert_batch(&self, rows: &[ShadowBlockRow]) -> anyhow::Result<usize>;
}

#[async_trait]
impl BlockInserter for ShadowBlockRepo {
    async fn insert_batch(&self, rows: &[ShadowBlockRow]) -> anyhow::Result<usize> {
        Self::insert_batch(self, rows).await
    }
}

/// Shadow indexer writer task.
#[derive(Debug)]
pub struct ShadowWriter {
    rx: mpsc::Receiver<ShadowBlockRow>,
    db_config: ShadowDbConfig,
    builder_version: String,
}

impl ShadowWriter {
    /// Spawns the writer task on the provided executor.
    pub fn spawn(
        executor: TaskExecutor,
        rx: mpsc::Receiver<ShadowBlockRow>,
        db_config: ShadowDbConfig,
        builder_version: String,
    ) {
        let writer = Self { rx, db_config, builder_version };
        executor.spawn_critical_task("shadow-indexer-writer", async move {
            writer.run().await;
        });
    }

    async fn run(mut self) {
        info!(target: "base::shadow-indexer", "Starting shadow indexer writer");

        let pool = match self.db_config.init_pool().await {
            Ok(pool) => pool,
            Err(error) => {
                error!(
                    target: "base::shadow-indexer",
                    error = ?error,
                    "Failed to initialize shadow indexer database pool"
                );
                panic!("failed to initialize shadow indexer database pool: {error:?}");
            }
        };
        let repo = ShadowBlockRepo::new(pool);
        let mut interval = interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut block_buffer = Vec::with_capacity(BATCH_SIZE);

        loop {
            tokio::select! {
                maybe_row = self.rx.recv() => {
                    match maybe_row {
                        Some(mut row) => {
                            self.stamp_row(&mut row);
                            block_buffer.push(row);
                            if block_buffer.len() >= BATCH_SIZE {
                                self.flush(&repo, &mut block_buffer).await;
                            }
                        }
                        None => {
                            self.flush(&repo, &mut block_buffer).await;
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    self.flush(&repo, &mut block_buffer).await;
                }
            }
        }
    }

    async fn flush(&self, repo: &dyn BlockInserter, buffer: &mut Vec<ShadowBlockRow>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();

        for attempt in 1..=MAX_FLUSH_ATTEMPTS {
            match repo.insert_batch(buffer).await {
                Ok(inserted) => {
                    info!(
                        target: "base::shadow-indexer",
                        inserted,
                        batch_size,
                        "Inserted shadow indexer rows"
                    );
                    buffer.clear();
                    return;
                }
                Err(error) => {
                    error!(
                        target: "base::shadow-indexer",
                        error = ?error,
                        batch_size,
                        "Failed to insert shadow indexer rows"
                    );
                    if attempt < MAX_FLUSH_ATTEMPTS {
                        sleep(RETRY_BACKOFF).await;
                    }
                }
            }
        }

        let (min_number, max_number) = buffer
            .iter()
            .fold((i64::MAX, i64::MIN), |acc, row| (acc.0.min(row.number), acc.1.max(row.number)));
        error!(
            target: "base::shadow-indexer",
            dropped = buffer.len(),
            min_block_number = min_number,
            max_block_number = max_number,
            "Dropping shadow indexer rows after failed retries"
        );
        buffer.clear();
    }

    fn stamp_row(&self, row: &mut ShadowBlockRow) {
        row.payload.builder_version = self.builder_version.clone();
    }
}

/// Spawns the shadow indexer writer task.
pub fn spawn_writer(
    executor: TaskExecutor,
    rx: mpsc::Receiver<ShadowBlockRow>,
    db_config: ShadowDbConfig,
    builder_version: String,
) {
    ShadowWriter::spawn(executor, rx, db_config, builder_version);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;
    use base_shadow_indexer_db::{ShadowBlockPayload, ShadowBlockRow, ShadowDbConfig};
    use chrono::{DateTime, Utc};
    use tokio::sync::mpsc;

    use super::{MAX_FLUSH_ATTEMPTS, MockBlockInserter, ShadowWriter};

    fn test_writer() -> ShadowWriter {
        let (_tx, rx) = mpsc::channel(1);
        ShadowWriter {
            rx,
            db_config: ShadowDbConfig {
                url: String::new(),
                max_connections: 1,
                connection_timeout: Duration::from_secs(1),
            },
            builder_version: "test-builder".to_string(),
        }
    }

    fn sample_row(number: i64, created_at: DateTime<Utc>) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: "hash".to_string(),
            reorged_out: false,
            canonical_hash: None,
            created_at,
            payload: ShadowBlockPayload {
                parent_hash: "parent".to_string(),
                timestamp: 0,
                tx_count: 0,
                gas_used: 0,
                state_root: "state".to_string(),
                builder_version: String::new(),
                transactions: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn flush_retries_then_drops_buffer() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut buffer = vec![sample_row(1, created_at)];
        writer.stamp_row(&mut buffer[0]);

        let mut repo = MockBlockInserter::new();
        repo.expect_insert_batch()
            .times(MAX_FLUSH_ATTEMPTS)
            .returning(|_| Err(anyhow!("insert failed")));

        writer.flush(&repo, &mut buffer).await;
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn flush_stamps_builder_version_without_mutating_created_at() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut buffer = vec![sample_row(7, created_at)];
        writer.stamp_row(&mut buffer[0]);

        let expected_created_at = created_at;
        let mut repo = MockBlockInserter::new();
        repo.expect_insert_batch().times(MAX_FLUSH_ATTEMPTS).returning(move |rows| {
            for row in rows {
                assert_eq!(row.created_at, expected_created_at);
                assert_eq!(row.payload.builder_version, "test-builder");
            }
            Err(anyhow!("insert failed"))
        });

        writer.flush(&repo, &mut buffer).await;
    }
}
