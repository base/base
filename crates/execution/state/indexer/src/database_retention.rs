//! Batched deletion of expired shadow block rows.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, PgPool, query, query_scalar};
use tokio::time::sleep;

/// Fleet-wide advisory lock key for the retention sweep.
///
/// Every shadow builder writes to the same database, so the key is a fixed arbitrary constant
/// rather than anything derived from node identity: they must all contend for the same lock.
/// Exported so operators can find the holder in `pg_locks`.
pub const SHADOW_RETENTION_LOCK_KEY: i64 = 0x0053_4841_444f_5701;

/// Rows deleted per transaction.
const DELETE_BATCH_ROWS: i64 = 2_000;

/// Pause between delete transactions, to leave the writer room against the same database.
const DELETE_BATCH_PAUSE: Duration = Duration::from_millis(100);

/// Ceiling on a single sweep, roughly a day and a half of ingest at a two-second block cadence.
///
/// A sweep that hits this stops early and resumes on the next tick, so a large backlog drains
/// over several sweeps instead of holding the database busy in one long run.
const MAX_ROWS_PER_SWEEP: u64 = 100_000;

/// Result of one retention sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowRetentionSweep {
    /// Rows deleted.
    pub deleted: u64,
    /// Delete transactions committed.
    pub batches: u32,
    /// Whether the sweep stopped at [`MAX_ROWS_PER_SWEEP`] with rows still expired.
    pub capped: bool,
}

/// Deletes shadow block rows that have outlived the retention period.
#[derive(Debug)]
pub struct ShadowRetentionRepo {
    pool: PgPool,
}

impl ShadowRetentionRepo {
    /// Creates a repository.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolves the retention cutoff against the database clock.
    ///
    /// Read from Postgres rather than the node so a skewed builder clock cannot widen or narrow
    /// the window.
    ///
    /// # Errors
    /// Returns an error when the period does not fit in seconds or the query fails.
    pub async fn cutoff(&self, period: Duration) -> Result<DateTime<Utc>> {
        let seconds = i64::try_from(period.as_secs())
            .context("shadow retention period does not fit in a signed 64-bit second count")?;

        query_scalar("SELECT now() - ($1::bigint * interval '1 second')")
            .bind(seconds)
            .fetch_one(&self.pool)
            .await
            .context("failed to resolve the shadow retention cutoff")
    }

    /// Deletes every row last written before `cutoff`, in bounded batches.
    ///
    /// Returns `None` when another builder in the fleet already holds the retention lock.
    ///
    /// # Errors
    /// Returns an error when the lock query or any delete batch fails.
    pub async fn sweep(&self, cutoff: DateTime<Utc>) -> Result<Option<ShadowRetentionSweep>> {
        let mut conn =
            self.pool.acquire().await.context("failed to acquire a shadow retention connection")?;

        // The advisory lock is held for the session, so the lock outlives any single statement.
        // Closing the connection on drop releases it even if a batch below fails midway.
        conn.close_on_drop();

        let acquired: bool = query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SHADOW_RETENTION_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await
            .context("failed to take the shadow retention advisory lock")?;

        if !acquired {
            return Ok(None);
        }

        let mut sweep = ShadowRetentionSweep::default();

        loop {
            let deleted = Self::delete_batch(&mut conn, cutoff).await?;
            if deleted == 0 {
                break;
            }

            sweep.deleted = sweep.deleted.saturating_add(deleted);
            sweep.batches = sweep.batches.saturating_add(1);

            if sweep.deleted >= MAX_ROWS_PER_SWEEP {
                sweep.capped = true;
                break;
            }

            sleep(DELETE_BATCH_PAUSE).await;
        }

        Ok(Some(sweep))
    }

    async fn delete_batch(conn: &mut PgConnection, cutoff: DateTime<Utc>) -> Result<u64> {
        let mut tx = conn.begin().await.context("failed to open a shadow retention batch")?;

        // Yield rather than queue behind a writer holding a conflicting lock; the rows are
        // retried on the next batch.
        query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await
            .context("failed to set the shadow retention lock timeout")?;
        query("SET LOCAL statement_timeout = '30s'")
            .execute(&mut *tx)
            .await
            .context("failed to set the shadow retention statement timeout")?;

        // `SKIP LOCKED` steps over rows the writer is upserting right now; they expire on a
        // later sweep. `MATERIALIZED` keeps the limit from being inlined into the delete.
        let deleted = query(
            "WITH expired AS MATERIALIZED ( \
               SELECT number FROM shadow_blocks \
               WHERE updated_at < $1 \
               ORDER BY updated_at, number \
               LIMIT $2 \
               FOR UPDATE SKIP LOCKED \
             ) \
             DELETE FROM shadow_blocks AS blocks \
             USING expired \
             WHERE blocks.number = expired.number",
        )
        .bind(cutoff)
        .bind(DELETE_BATCH_ROWS)
        .execute(&mut *tx)
        .await
        .context("failed to delete a batch of expired shadow blocks")?
        .rows_affected();

        tx.commit().await.context("failed to commit a shadow retention batch")?;

        Ok(deleted)
    }
}
