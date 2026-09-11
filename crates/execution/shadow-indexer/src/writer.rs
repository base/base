use std::{collections::HashSet, time::Duration};

use async_trait::async_trait;
use base_shadow_indexer_db::{
    ShadowBlockRepo, ShadowBlockRow, ShadowDbConfig, ShadowFlushOutcome, ShadowWrite,
};
use reth_tasks::TaskExecutor;
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, sleep},
};
use tracing::{error, info};

use crate::ShadowWriterMetrics;

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_FLUSH_ATTEMPTS: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

#[cfg_attr(test, mockall::automock)]
#[async_trait]
trait BlockInserter: Send + Sync {
    async fn flush(&self, writes: &[ShadowWrite]) -> anyhow::Result<ShadowFlushOutcome>;
}

#[async_trait]
impl BlockInserter for ShadowBlockRepo {
    async fn flush(&self, writes: &[ShadowWrite]) -> anyhow::Result<ShadowFlushOutcome> {
        Self::flush(self, writes).await
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
        let mut batch: Vec<ShadowWrite> = Vec::with_capacity(BATCH_SIZE);

        loop {
            tokio::select! {
                maybe_write = self.rx.recv() => {
                    match maybe_write {
                        Some(mut write) => {
                            if let ShadowWrite::Reorged(row) = &mut write {
                                self.stamp_row(row);
                            }
                            batch.push(write);
                            ShadowWriterMetrics::channel_depth().set(self.rx.len() as f64);
                            ShadowWriterMetrics::buffer_size().set(batch.len() as f64);
                            if batch.len() >= BATCH_SIZE {
                                self.flush(&repo, &mut batch, "batch_full").await;
                            }
                        }
                        None => {
                            self.flush(&repo, &mut batch, "channel_closed").await;
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    ShadowWriterMetrics::channel_depth().set(self.rx.len() as f64);
                    self.flush(&repo, &mut batch, "interval").await;
                }
            }
        }
    }

    async fn flush(
        &self,
        repo: &dyn BlockInserter,
        batch: &mut Vec<ShadowWrite>,
        trigger: &'static str,
    ) {
        if batch.is_empty() {
            return;
        }

        let batch_size = batch.len();
        ShadowWriterMetrics::flushes(trigger).increment(1);
        let _flush_timer = base_metrics::timed!(ShadowWriterMetrics::flush_duration_seconds());

        for attempt in 1..=MAX_FLUSH_ATTEMPTS {
            match repo.flush(batch).await {
                Ok(outcome) => {
                    info!(
                        target: "base::shadow-indexer",
                        inserted = outcome.rows_written,
                        reconciled = outcome.rows_reconciled,
                        batch_size,
                        "Inserted shadow indexer rows"
                    );
                    ShadowWriterMetrics::rows_inserted().increment(outcome.rows_written as u64);
                    batch.clear();
                    ShadowWriterMetrics::buffer_size().set(0.0);
                    return;
                }
                Err(error) => {
                    error!(
                        target: "base::shadow-indexer",
                        error = ?error,
                        batch_size,
                        "Failed to insert shadow indexer rows"
                    );
                    ShadowWriterMetrics::flush_failures().increment(1);
                    if attempt < MAX_FLUSH_ATTEMPTS {
                        sleep(RETRY_BACKOFF).await;
                    }
                }
            }
        }

        let (min_number, max_number) =
            batch.iter().fold((i64::MAX, i64::MIN), |acc, write| match write {
                ShadowWrite::Reorged(row) => (acc.0.min(row.number), acc.1.max(row.number)),
                ShadowWrite::Canonical(_) => acc,
            });
        let dropped = Self::drop_rows_retaining_canonical(batch);
        error!(
            target: "base::shadow-indexer",
            dropped,
            retained_canonical = batch.len(),
            min_block_number = min_number,
            max_block_number = max_number,
            "Dropping shadow indexer rows after failed retries"
        );
        ShadowWriterMetrics::rows_dropped().increment(dropped as u64);
        ShadowWriterMetrics::buffer_size().set(batch.len() as f64);
    }

    /// Discards the block rows of a failed batch but keeps its canonical refs for the next flush.
    ///
    /// A dropped row only costs that block's metrics, but a dropped ref strands a row already
    /// persisted by an earlier flush at `NULL` forever: the reader skips it on every poll and
    /// nothing revisits it. Refs collapse to the last one per height, which is what the repository
    /// would do with them anyway, so a long outage grows the buffer by distinct heights rather than
    /// by every committed block.
    fn drop_rows_retaining_canonical(batch: &mut Vec<ShadowWrite>) -> usize {
        let dropped = batch.iter().filter(|write| matches!(write, ShadowWrite::Reorged(_))).count();

        let mut seen: HashSet<i64> = HashSet::new();
        let mut retained: Vec<ShadowWrite> = batch
            .drain(..)
            .rev()
            .filter(|write| match write {
                ShadowWrite::Canonical(entry) => seen.insert(entry.number),
                ShadowWrite::Reorged(_) => false,
            })
            .collect();
        retained.reverse();
        *batch = retained;

        dropped
    }

    fn stamp_row(&self, row: &mut ShadowBlockRow) {
        row.payload.builder_version = self.builder_version.clone();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;
    use base_shadow_indexer_db::{
        PgConnectionParams, ShadowBlockPayload, ShadowBlockRow, ShadowCanonicalRef, ShadowDbConfig,
        ShadowFlushOutcome,
    };
    use chrono::{DateTime, Utc};
    use reth_primitives_traits::RecoveredBlock;
    use tokio::sync::mpsc;

    use super::{MAX_FLUSH_ATTEMPTS, MockBlockInserter, ShadowWrite, ShadowWriter};

    fn test_writer() -> ShadowWriter {
        let (_tx, rx) = mpsc::channel(1);
        ShadowWriter {
            rx,
            db_config: ShadowDbConfig {
                connection: PgConnectionParams::default(),
                max_connections: 1,
                connection_timeout: Duration::from_secs(1),
            },
            builder_version: "test-builder".to_string(),
        }
    }

    fn sample_row(number: i64, created_at: DateTime<Utc>) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: "0xhash".to_owned(),
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

    fn reorged(number: i64, created_at: DateTime<Utc>) -> ShadowWrite {
        ShadowWrite::Reorged(Box::new(sample_row(number, created_at)))
    }

    fn canonical(number: i64) -> ShadowWrite {
        ShadowWrite::Canonical(ShadowCanonicalRef { number, hash: "0xcanonical".to_owned() })
    }

    #[tokio::test]
    async fn flush_retries_then_drops_rows() {
        let writer = test_writer();
        let mut batch = vec![reorged(1, Utc::now())];

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(MAX_FLUSH_ATTEMPTS).returning(|_| Err(anyhow!("insert failed")));

        writer.flush(&repo, &mut batch, "batch_full").await;
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn flush_keeps_canonical_refs_when_rows_are_dropped() {
        let writer = test_writer();
        let mut batch = vec![reorged(1, Utc::now()), canonical(9), canonical(9), canonical(10)];

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(MAX_FLUSH_ATTEMPTS).returning(|_| Err(anyhow!("insert failed")));

        writer.flush(&repo, &mut batch, "batch_full").await;

        let retained: Vec<i64> = batch
            .iter()
            .filter_map(|write| match write {
                ShadowWrite::Canonical(entry) => Some(entry.number),
                ShadowWrite::Reorged(_) => None,
            })
            .collect();
        assert_eq!(retained, vec![9, 10], "refs survive so they cannot strand a persisted row");
    }

    #[tokio::test]
    async fn flush_stamps_builder_version_without_mutating_created_at() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut batch = vec![reorged(7, created_at)];
        if let ShadowWrite::Reorged(row) = &mut batch[0] {
            writer.stamp_row(row);
        }

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(MAX_FLUSH_ATTEMPTS).returning(move |writes| {
            for write in writes {
                let ShadowWrite::Reorged(row) = write else { continue };
                assert_eq!(row.created_at, created_at);
                assert_eq!(row.payload.builder_version, "test-builder");
            }
            Err(anyhow!("insert failed"))
        });

        writer.flush(&repo, &mut batch, "batch_full").await;
    }

    #[tokio::test]
    async fn flush_skips_database_round_trip_when_batch_is_empty() {
        let writer = test_writer();
        let mut batch = Vec::new();

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().never();

        writer.flush(&repo, &mut batch, "interval").await;
    }

    #[tokio::test]
    async fn flush_preserves_the_order_writes_arrived_in() {
        let writer = test_writer();
        let created_at = Utc::now();
        let mut batch = vec![reorged(9, created_at), canonical(9), reorged(9, created_at)];

        let mut repo = MockBlockInserter::new();
        repo.expect_flush().times(1).returning(|writes| {
            let kinds: Vec<bool> =
                writes.iter().map(|write| matches!(write, ShadowWrite::Reorged(_))).collect();
            assert_eq!(kinds, vec![true, false, true], "the repository sees the arrival order");
            Ok(ShadowFlushOutcome { rows_written: 2, rows_reconciled: 1 })
        });

        writer.flush(&repo, &mut batch, "batch_full").await;
        assert!(batch.is_empty());
    }
}
