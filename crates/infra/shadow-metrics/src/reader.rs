//! Polling and emission for persisted shadow blocks.

use std::time::Duration;

use anyhow::{Result, ensure};
use base_shadow_indexer_db::{ShadowBlockCursor, ShadowBlockRepo, ShadowMetricsCursorRepo};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::{ShadowBlockStats, ShadowMetrics, ShadowMetricsStore};

/// Default poll interval in seconds.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;

/// Default row limit per poll.
pub const DEFAULT_MAX_ROWS_PER_POLL: u32 = 1_000;

/// Reader settings.
#[derive(Clone, Copy, Debug)]
pub struct ShadowMetricsReaderConfig {
    /// Poll interval.
    pub poll_interval: Duration,
    /// Row limit per poll.
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

/// Polls finalized shadow blocks and emits metrics.
///
/// `updated_at` revisits rows after reconciliation; block-number cursors classify too early.
#[derive(Debug)]
pub struct ShadowMetricsReader {
    block_repo: ShadowBlockRepo,
    cursor_repo: ShadowMetricsCursorRepo,
    config: ShadowMetricsReaderConfig,
    cursor: ShadowBlockCursor,
    latest_block_number: Option<i64>,
}

impl ShadowMetricsReader {
    /// Creates a reader, resuming a cursor or starting at current table tip.
    ///
    /// # Errors
    /// Returns an error for invalid settings or cursor I/O failure.
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
                    "resuming shadow metrics reader from persisted cursor"
                );
                cursor
            }
            None => {
                // `shadow_blocks` retains only reorged-out blocks, so the newest row is the last
                // reorg rather than a seconds-old canonical block. This tip is expected to be
                // hours or days old on a chain that reorgs rarely; staleness is not a fault.
                let cursor = block_repo.max_cursor().await?.map_or_else(
                    || {
                        // Ordinary first boot: a chain with no reorgs yet stores no rows at all.
                        info!(
                            "no persisted shadow metrics cursor and no recorded reorgs; starting at genesis"
                        );
                        ShadowBlockCursor::genesis()
                    },
                    |cursor| {
                        info!(
                            updated_at = %cursor.updated_at,
                            number = cursor.number,
                            "no persisted shadow metrics cursor; starting at last recorded reorg"
                        );
                        cursor
                    },
                );

                // Persist now so restart before first batch cannot skip intervening rows.
                cursor_repo.store(&cursor).await?;
                cursor
            }
        };

        Ok(Self { block_repo, cursor_repo, config, cursor, latest_block_number: None })
    }

    /// Polls one batch and returns emitted statistics.
    ///
    /// Typed payload incompatibility fails the whole fetch and stalls at this cursor.
    ///
    /// # Errors
    /// Returns an error when fetch or cursor persistence fails.
    pub async fn poll_once(&mut self) -> Result<Vec<ShadowBlockStats>> {
        let rows = self
            .block_repo
            .list_since(&self.cursor, i64::from(self.config.max_rows_per_poll))
            .await?;
        let mut emitted = Vec::with_capacity(rows.len());
        let mut next_cursor = self.cursor.clone();

        for row in rows {
            let cursor = row.cursor();
            match row.canonical_hash.as_ref() {
                None => ShadowMetrics::reverted_blocks_total().increment(1),
                Some(_) => {
                    let stats = ShadowBlockStats::from_row(&row);
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
            }
            // Every fetched row advances the cursor, including unwinds.
            next_cursor = cursor;
        }

        if next_cursor != self.cursor {
            // Persist after emission: duplicate retries are safer than permanent metric holes.
            self.cursor_repo.store(&next_cursor).await?;
            self.cursor = next_cursor;
        }

        Ok(emitted)
    }

    /// Runs until cancelled, counting and retrying poll failures.
    ///
    /// # Errors
    /// Polling errors are handled internally.
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
