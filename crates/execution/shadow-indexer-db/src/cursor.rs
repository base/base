//! Shadow metrics cursor state and persistence.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, query, query_as};

/// Composite position in the `shadow_blocks` update stream.
/// `(number, hash)` breaks transaction-wide `updated_at` ties so capped polls cannot skip rows.
/// Safety requires serialized writers; concurrent commits can land behind the watermark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowBlockCursor {
    /// Row update time.
    pub updated_at: DateTime<Utc>,
    /// First tie-breaker.
    pub number: i64,
    /// Final tie-breaker.
    pub hash: Vec<u8>,
}

impl ShadowBlockCursor {
    /// Returns a Postgres-safe scan lower bound.
    /// First boot should use `ShadowBlockRepo::max_cursor()` to avoid historical replay.
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
    /// Creates a repository.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Loads the persisted cursor.
    /// # Errors
    /// Returns an error when the query fails.
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

    /// Stores the cursor without moving it backwards.
    /// The guard limits rolling-deploy replay; readers must remain singleton to avoid duplicates.
    /// # Errors
    /// Returns an error when the upsert fails.
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
