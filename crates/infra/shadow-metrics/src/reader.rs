//! Polling reader for persisted shadow block metrics.

use std::time::Duration;

use anyhow::{Result, ensure};
use base_shadow_indexer_db::{ShadowBlockCursor, ShadowBlockRepo, ShadowMetricsCursorRepo};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::{ShadowBlockStats, ShadowMetrics, ShadowMetricsStore};

/// Default interval in seconds between shadow metrics polls.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;

/// Default maximum number of shadow block rows fetched by one poll.
pub const DEFAULT_MAX_ROWS_PER_POLL: u32 = 1_000;

/// Runtime settings for [`ShadowMetricsReader`].
#[derive(Clone, Copy, Debug)]
pub struct ShadowMetricsReaderConfig {
    /// Interval between polling attempts.
    pub poll_interval: Duration,
    /// Maximum number of rows fetched by one polling attempt.
    pub max_rows_per_poll: u32,
}

impl Default for ShadowMetricsReaderConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            max_rows_per_poll: DEFAULT_MAX_ROWS_PER_POLL,
        }
    }
}

/// Polls finalized shadow block rows and emits their metrics.
///
/// Cursor safety depends on `ShadowWriter::run` flushing sequentially with only one
/// `insert_batch` in flight. Concurrent writers can commit rows behind this reader's
/// `now()`-based watermark. If the writer becomes concurrent, this design silently
/// loses rows and must use a snapshot- or sequence-based watermark instead.
#[derive(Debug)]
pub struct ShadowMetricsReader {
    block_repo: ShadowBlockRepo,
    cursor_repo: ShadowMetricsCursorRepo,
    config: ShadowMetricsReaderConfig,
    cursor: ShadowBlockCursor,
    latest_block_number: Option<u64>,
}

impl ShadowMetricsReader {
    /// Creates a reader and resolves its starting cursor.
    ///
    /// A persisted cursor provides restart continuity. Without one, the reader starts
    /// at the current table tip to avoid replaying historical rows, or at genesis when
    /// the table is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or the starting cursor cannot be
    /// loaded from Postgres.
    pub async fn new(store: ShadowMetricsStore, config: ShadowMetricsReaderConfig) -> Result<Self> {
        ensure!(!config.poll_interval.is_zero(), "poll interval must be greater than zero");
        ensure!(config.max_rows_per_poll > 0, "max rows per poll must be greater than zero");

        let block_repo = ShadowBlockRepo::new(store.pool().clone());
        let cursor_repo = ShadowMetricsCursorRepo::new(store.pool().clone());
        let cursor = match cursor_repo.load().await? {
            Some(cursor) => {
                info!(
                    updated_at = %cursor.updated_at,
                    number = cursor.number,
                    hash = ?cursor.hash,
                    "resuming shadow metrics reader from persisted cursor"
                );
                cursor
            }
            None => {
                let cursor = match block_repo.max_cursor().await? {
                    Some(cursor) => {
                        info!(
                            updated_at = %cursor.updated_at,
                            number = cursor.number,
                            hash = ?cursor.hash,
                            "no persisted shadow metrics cursor; starting at current table tip"
                        );
                        cursor
                    }
                    None => {
                        info!(
                            "no persisted shadow metrics cursor and shadow block table is empty; starting at genesis"
                        );
                        ShadowBlockCursor::genesis()
                    }
                };

                // Persist the first-boot boundary immediately. Otherwise, a restart before the
                // first non-empty batch re-resolves to the then-current tip and silently skips
                // rows written in between, violating restart continuity rather than merely
                // omitting historical backfill.
                cursor_repo.store(&cursor).await?;
                cursor
            }
        };

        Ok(Self { block_repo, cursor_repo, config, cursor, latest_block_number: None })
    }

    /// Polls one batch, emits metrics, and returns the stats it emitted.
    ///
    /// Pipeline unwinds and undecodable payloads advance the cursor but produce no
    /// returned stats. Database failures leave the in-memory cursor unchanged so the
    /// next call retries the same batch.
    ///
    /// # Errors
    ///
    /// Returns an error if fetching rows or persisting the resulting cursor fails.
    pub async fn poll_once(&mut self) -> Result<Vec<ShadowBlockStats>> {
        let rows = self
            .block_repo
            .list_reorged_since(&self.cursor, i64::from(self.config.max_rows_per_poll))
            .await?;
        let mut emitted = Vec::with_capacity(rows.len());
        let mut next_cursor = self.cursor.clone();

        for row in rows {
            let cursor = row.cursor();
            match row.canonical_hash.as_ref() {
                None => ShadowMetrics::reverted_blocks_total().increment(1),
                Some(_) => match row.decode_payload() {
                    Ok(payload) => {
                        let stats = ShadowBlockStats::from_payload(&payload);
                        ShadowMetrics::gas_used(stats.builder_version.clone())
                            .record(stats.gas_used as f64);
                        ShadowMetrics::transaction_count(stats.builder_version.clone())
                            .record(stats.transaction_count as f64);
                        ShadowMetrics::priority_fee_inversions(stats.builder_version.clone())
                            .record(stats.priority_fee_inversions as f64);
                        ShadowMetrics::blocks_inspected_total().increment(1);
                        if stats.non_deposit_tx_count == 0 {
                            ShadowMetrics::empty_blocks_total().increment(1);
                        }

                        if self.latest_block_number.is_none_or(|latest| stats.number > latest) {
                            self.latest_block_number = Some(stats.number);
                            ShadowMetrics::latest_block_number().set(stats.number as f64);
                        }
                        emitted.push(stats);
                    }
                    Err(error) => {
                        ShadowMetrics::payload_decode_errors_total().increment(1);
                        error!(
                            error = %error,
                            number = row.number,
                            hash = ?row.hash,
                            "failed to decode shadow block payload; skipping"
                        );
                    }
                },
            }
            next_cursor = cursor;
        }

        if next_cursor != self.cursor {
            // Emit before persisting for at-least-once delivery. A crash or store failure
            // re-emits this batch; persisting first would create permanent metric holes.
            self.cursor_repo.store(&next_cursor).await?;
            self.cursor = next_cursor;
        }

        Ok(emitted)
    }

    /// Runs the poll loop until cancelled by dropping this future.
    ///
    /// Poll failures are counted and retried on the next delayed tick. This method
    /// never returns an error, so transient Postgres failures cannot crash-loop the
    /// service.
    ///
    /// # Errors
    ///
    /// This method handles all polling errors internally and does not return an error.
    pub async fn run(mut self) -> Result<()> {
        let mut interval = interval(self.config.poll_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if let Err(error) = self.poll_once().await {
                ShadowMetrics::poll_errors_total().increment(1);
                error!(error = %error, "shadow metrics poll failed; retrying on next tick");
            }
        }
    }
}
