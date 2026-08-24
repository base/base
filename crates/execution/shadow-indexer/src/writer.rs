use std::time::Duration;

use async_trait::async_trait;
use base_shadow_indexer_db::{
    ShadowBlockRepo, ShadowBlockRow, ShadowCanonicalRef, ShadowDbConfig, ShadowFlushOutcome,
};
use reth_tasks::TaskExecutor;
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, sleep},
};
use tracing::{error, info};

use crate::ShadowWrite;

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_FLUSH_ATTEMPTS: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

#[cfg_attr(test, mockall::automock)]
#[async_trait]
trait BlockInserter: Send + Sync {
    async fn flush(
        &self,
        rows: &[ShadowBlockRow],
        canonical: &[ShadowCanonicalRef],
    ) -> anyhow::Result<ShadowFlushOutcome>;
}

#[async_trait]
impl BlockInserter for ShadowBlockRepo {
    async fn flush(
        &self,
        rows: &[ShadowBlockRow],
        canonical: &[ShadowCanonicalRef],
    ) -> anyhow::Result<ShadowFlushOutcome> {
        Self::flush(self, rows, canonical).await
    }
}

#[derive(Debug, Default)]
struct ShadowBatch {
    rows: Vec<ShadowBlockRow>,
    canonical: Vec<ShadowCanonicalRef>,
}

impl ShadowBatch {
    const fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.canonical.is_empty()
    }

    const fn len(&self) -> usize {
        self.rows.len().saturating_add(self.canonical.len())
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.canonical.clear();
    }
}

/// Shadow indexer writer task.
#[derive(Debug)]
pub struct ShadowWriter {
    rx: mpsc::Receiver<ShadowWrite>,
    db_config: ShadowDbConfig,
    builder_version: String,
}

impl ShadowWriter {
    /// Spawns the writer task on the provided executor.
    pub fn spawn(
        executor: TaskExecutor,
        rx: mpsc::Receiver<ShadowWrite>,
        db_config: ShadowDbConfig,
        builder_version: String,
    ) {
        let writer = Self { rx, db_config, builder_version };
        // Spawned as a critical task on purpose: this runs only on shadow canary nodes, where an
        // unrecoverable writer failure (pool init panic, or the task ending) should fail-fast and
        // take the node down rather than let it run without shadow indexing. On normal nodes the
        // extension is disabled and this is never spawned.
        executor.spawn_critical_task("shadow-indexer-writer", async move {
            writer.run().await;
        });
    }

    async fn run(mut self) {
        info!(target: "base::shadow-indexer", "Starting shadow indexer writer");

        let pool = self.db_config.init_pool().await.unwrap_or_else(|error| {
            panic!("failed to initialize shadow indexer database pool: {error:?}")
        });
        let repo = ShadowBlockRepo::new(pool);
        let mut interval = interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut batch = ShadowBatch::default();

        loop {
            tokio::select! {
                maybe_write = self.rx.recv() => {
                    match maybe_write {
                        Some(write) => {
                            match write {
                                ShadowWrite::Reorged(row) => {
                                    let mut row = *row;
                                    self.stamp_row(&mut row);
                                    batch.rows.push(row);
                                }
                                ShadowWrite::Canonical(canonical) => {
                                    batch.canonical.push(canonical);
                                }
                            }
                            if batch.len() >= BATCH_SIZE {
                                self.flush(&repo, &mut batch).await;
                            }
                        }
                        None => {
                            self.flush(&repo, &mut batch).await;
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    self.flush(&repo, &mut batch).await;
                }
            }
        }
    }

    async fn flush(&self, repo: &dyn BlockInserter, batch: &mut ShadowBatch) {
        if batch.is_empty() {
            return;
        }

        let batch_size = batch.rows.len();
        let canonical_size = batch.canonical.len();

        for attempt in 1..=MAX_FLUSH_ATTEMPTS {
            match repo.flush(&batch.rows, &batch.canonical).await {
                Ok(outcome) => {
                    info!(
                        target: "base::shadow-indexer",
                        inserted = outcome.rows_written,
                        reconciled = outcome.rows_reconciled,
                        batch_size,
                        canonical_size,
                        "Inserted shadow indexer rows"
                    );
                    batch.clear();
                    return;
                }
                Err(error) => {
                    error!(
                        target: "base::shadow-indexer",
                        error = ?error,
                        batch_size,
                        canonical_size,
                        "Failed to insert shadow indexer rows"
                    );
                    if attempt < MAX_FLUSH_ATTEMPTS {
                        sleep(RETRY_BACKOFF).await;
                    }
                }
            }
        }

        let (min_number, max_number) = batch
            .rows
            .iter()
            .fold((i64::MAX, i64::MIN), |acc, row| (acc.0.min(row.number), acc.1.max(row.number)));
        error!(
            target: "base::shadow-indexer",
            dropped = batch.rows.len(),
            dropped_canonical = canonical_size,
            min_block_number = min_number,
            max_block_number = max_number,
            "Dropping shadow indexer rows after failed retries"
        );
        batch.clear();
    }

    fn stamp_row(&self, row: &mut ShadowBlockRow) {
        row.payload.builder_version = self.builder_version.clone();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;
    use base_shadow_indexer_db::{ShadowBlockPayload, ShadowBlockRow, ShadowDbConfig};
    use chrono::{DateTime, Utc};
    use reth_primitives_traits::RecoveredBlock;
    use tokio::sync::mpsc;

    use super::{MAX_FLUSH_ATTEMPTS, MockBlockInserter, ShadowBatch, ShadowWriter};

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
            hash: b"hash".to_vec(),
            canonical_hash: None,
            created_at,
            updated_at: created_at,
            payload: ShadowBlockPayload {
                builder_version: String::new(),
                block: RecoveredBlock::default(),
                receipts: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn flush_retries_then_drops_batch() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut batch =
            ShadowBatch { rows: vec![sample_row(1, created_at)], canonical: Vec::new() };
        writer.stamp_row(&mut batch.rows[0]);

        let mut repo = MockBlockInserter::new();
        repo.expect_flush()
            .times(MAX_FLUSH_ATTEMPTS)
            .returning(|_, _| Err(anyhow!("insert failed")));

        writer.flush(&repo, &mut batch).await;
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn flush_stamps_builder_version_without_mutating_created_at() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut batch =
            ShadowBatch { rows: vec![sample_row(7, created_at)], canonical: Vec::new() };
        writer.stamp_row(&mut batch.rows[0]);

        let expected_created_at = created_at;
        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(MAX_FLUSH_ATTEMPTS).returning(move |rows, _| {
            for row in rows {
                assert_eq!(row.created_at, expected_created_at);
                assert_eq!(row.payload.builder_version, "test-builder");
            }
            Err(anyhow!("insert failed"))
        });

        writer.flush(&repo, &mut batch).await;
    }

    #[tokio::test]
    async fn flush_skips_database_round_trip_when_batch_is_empty() {
        let writer = test_writer();
        let mut batch = ShadowBatch::default();

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().never();

        writer.flush(&repo, &mut batch).await;
    }

    #[tokio::test]
    async fn flush_forwards_canonical_refs_without_rows() {
        let writer = test_writer();
        let mut batch = ShadowBatch {
            rows: Vec::new(),
            canonical: vec![base_shadow_indexer_db::ShadowCanonicalRef {
                number: 9,
                hash: b"canonical".to_vec(),
            }],
        };

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(1).returning(|rows, canonical| {
            assert!(rows.is_empty(), "canonical-only flush carries no rows");
            assert_eq!(canonical.len(), 1);
            assert_eq!(canonical[0].number, 9);
            Ok(base_shadow_indexer_db::ShadowFlushOutcome { rows_written: 0, rows_reconciled: 1 })
        });

        writer.flush(&repo, &mut batch).await;
        assert!(batch.is_empty());
    }
}
