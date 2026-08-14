//! Cursor state and persistence for shadow metrics reads.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, query, query_as};

/// Stable, totally-ordered position in the `shadow_blocks` update stream.
///
/// `updated_at` alone is not unique: Postgres `now()` is transaction-start time, so
/// every row written by one `insert_batch` transaction shares a timestamp. The
/// `(number, hash)` primary key breaks ties, making the ordering total.
///
/// This cursor is safe only while `insert_batch` transactions are serialized. With
/// concurrent writers, a transaction can commit after a newer transaction-start
/// timestamp has already advanced the cursor, leaving its rows behind the watermark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowBlockCursor {
    /// Last-write timestamp of the row.
    pub updated_at: DateTime<Utc>,
    /// Block number used as the first timestamp tie-breaker.
    pub number: i64,
    /// Block hash used as the final timestamp tie-breaker.
    pub hash: Vec<u8>,
}

impl ShadowBlockCursor {
    /// Returns the Unix epoch as the effective lower bound for stream scans.
    ///
    /// Shadow block rows are always newer than 1970. The epoch is used instead of
    /// `DateTime::MIN_UTC` because Postgres `TIMESTAMPTZ` cannot represent chrono's
    /// minimum timestamp. First-boot callers should still prefer
    /// `ShadowBlockRepo::max_cursor()` to avoid replaying existing rows.
    #[must_use]
    pub const fn genesis() -> Self {
        Self { updated_at: DateTime::<Utc>::UNIX_EPOCH, number: 0, hash: Vec::new() }
    }
}

/// Repository for the singleton shadow metrics cursor.
#[derive(Debug)]
pub struct ShadowMetricsCursorRepo {
    pool: PgPool,
}

impl ShadowMetricsCursorRepo {
    /// Creates a new repository backed by the provided pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Loads the persisted shadow metrics cursor, if present.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load(&self) -> Result<Option<ShadowBlockCursor>> {
        let row = query_as::<_, (DateTime<Utc>, i64, Vec<u8>)>(
            "SELECT last_updated_at, last_number, last_hash \
             FROM shadow_metrics_cursor WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("failed to load shadow metrics cursor")?;

        Ok(row.map(|(updated_at, number, hash)| ShadowBlockCursor { updated_at, number, hash }))
    }

    /// Stores the shadow metrics cursor in the singleton row without moving it backwards.
    ///
    /// The monotonic guard prevents concurrent readers during a rolling deployment from
    /// rewinding their shared cursor. This bounds duplicate replay but does not make concurrent
    /// readers correct: they still double-emit, so deployments should run a single reader.
    ///
    /// # Errors
    ///
    /// Returns an error if the upsert fails.
    pub async fn store(&self, at: &ShadowBlockCursor) -> Result<()> {
        query(
            "INSERT INTO shadow_metrics_cursor \
             (id, last_updated_at, last_number, last_hash, updated_at) \
             VALUES (1, $1, $2, $3, now()) \
             ON CONFLICT (id) DO UPDATE SET \
             last_updated_at = EXCLUDED.last_updated_at, \
             last_number = EXCLUDED.last_number, \
             last_hash = EXCLUDED.last_hash, \
             updated_at = now() \
             WHERE (shadow_metrics_cursor.last_updated_at, \
                    shadow_metrics_cursor.last_number, \
                    shadow_metrics_cursor.last_hash) \
                 < (EXCLUDED.last_updated_at, EXCLUDED.last_number, EXCLUDED.last_hash)",
        )
        .bind(at.updated_at)
        .bind(at.number)
        .bind(&at.hash)
        .execute(&self.pool)
        .await
        .context("failed to store shadow metrics cursor")?;

        Ok(())
    }
}
