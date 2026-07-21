use anyhow::{Context, Result};
use sqlx::{PgPool, Postgres, QueryBuilder, query_as};

use crate::ShadowBlockRow;

/// Repository for shadow canary block persistence.
#[derive(Debug)]
pub struct ShadowBlockRepo {
    pool: PgPool,
}

impl ShadowBlockRepo {
    /// Create a new repository backed by the provided pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a batch of shadow block rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn insert_batch(&self, rows: &[ShadowBlockRow]) -> Result<usize> {
        const CHUNK_SIZE: usize = 5_000;

        if rows.is_empty() {
            return Ok(0);
        }

        let mut inserted = 0usize;

        for chunk in rows.chunks(CHUNK_SIZE) {
            let mut query_builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
                "INSERT INTO shadow_blocks \
                 (number, hash, parent_hash, timestamp, tx_count, gas_used, da_bytes, \
                  state_root, build_latency_ms, deadline_miss, fb_count, panicked, \
                  reorged_out, canonical_hash, builder_version, created_at) ",
            );

            query_builder.push_values(chunk, |mut row, entry| {
                row.push_bind(entry.number)
                    .push_bind(&entry.hash)
                    .push_bind(&entry.parent_hash)
                    .push_bind(entry.timestamp)
                    .push_bind(entry.tx_count)
                    .push_bind(entry.gas_used)
                    .push_bind(entry.da_bytes)
                    .push_bind(&entry.state_root)
                    .push_bind(entry.build_latency_ms)
                    .push_bind(entry.deadline_miss)
                    .push_bind(entry.fb_count)
                    .push_bind(entry.panicked)
                    .push_bind(entry.reorged_out)
                    .push_bind(&entry.canonical_hash)
                    .push_bind(&entry.builder_version)
                    .push_bind(entry.created_at);
            });

            query_builder.push(
                " ON CONFLICT (number, hash) DO UPDATE SET \
                 reorged_out = EXCLUDED.reorged_out, \
                 canonical_hash = EXCLUDED.canonical_hash",
            );

            let result = query_builder
                .build()
                .execute(&self.pool)
                .await
                .context("failed to insert shadow block batch")?;

            inserted = inserted
                .saturating_add(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX));
        }

        Ok(inserted)
    }

    /// Lists shadow block rows with block numbers in the provided inclusive range.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn list_by_number_range(&self, start: i64, end: i64) -> Result<Vec<ShadowBlockRow>> {
        let rows = query_as::<_, ShadowBlockRow>(
            "SELECT * FROM shadow_blocks WHERE number BETWEEN $1 AND $2 ORDER BY number, created_at",
        )
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await
        .context("failed to list shadow blocks by number range")?;

        Ok(rows)
    }
}
