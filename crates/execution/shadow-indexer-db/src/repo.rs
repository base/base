use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::{PgPool, Postgres, QueryBuilder, query_as, types::Json};

use crate::ShadowBlockRow;

/// Repository for shadow indexer block persistence.
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
        // 6 columns per row are bound. Postgres caps a single statement at 65_535
        // bind parameters, so keep chunks below 65_535 / 6 ≈ 10_922 rows.
        const CHUNK_SIZE: usize = 4_000;

        if rows.is_empty() {
            return Ok(0);
        }

        // Collapse duplicate `(number, hash)` rows within the batch, keeping the
        // last occurrence. A single `INSERT ... ON CONFLICT DO UPDATE` cannot
        // touch the same conflict key twice (Postgres error 21000), which would
        // otherwise reject the entire batch when a block is committed and then
        // reorged out inside one flush window.
        let deduped = Self::dedupe_last_write_wins(rows);

        let mut inserted = 0usize;

        for chunk in deduped.chunks(CHUNK_SIZE) {
            let mut query_builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
                "INSERT INTO shadow_blocks \
                 (number, hash, reorged_out, canonical_hash, created_at, payload) ",
            );

            query_builder.push_values(chunk, |mut row, entry| {
                row.push_bind(entry.number)
                    .push_bind(&entry.hash)
                    .push_bind(entry.reorged_out)
                    .push_bind(&entry.canonical_hash)
                    .push_bind(entry.created_at)
                    .push_bind(Json(&entry.payload));
            });

            query_builder.push(
                " ON CONFLICT (number, hash) DO UPDATE SET \
                 reorged_out = EXCLUDED.reorged_out, \
                 canonical_hash = EXCLUDED.canonical_hash, \
                 payload = EXCLUDED.payload, \
                 updated_at = now()",
            );

            let result = query_builder
                .build()
                .execute(&self.pool)
                .await
                .context("failed to insert shadow block batch")?;

            inserted = inserted.saturating_add(result.rows_affected() as usize);
        }

        Ok(inserted)
    }

    fn dedupe_last_write_wins(rows: &[ShadowBlockRow]) -> Vec<&ShadowBlockRow> {
        let mut by_key: HashMap<(i64, &[u8]), &ShadowBlockRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            by_key.insert((row.number, row.hash.as_slice()), row);
        }
        by_key.into_values().collect()
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use reth_primitives_traits::RecoveredBlock;

    use super::*;
    use crate::ShadowBlockPayload;

    fn sample_row(number: i64, hash: &[u8], reorged_out: bool) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: hash.to_vec(),
            reorged_out,
            canonical_hash: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            payload: ShadowBlockPayload {
                builder_version: String::new(),
                block: RecoveredBlock::default(),
                receipts: Vec::new(),
            },
        }
    }

    #[test]
    fn dedupe_collapses_duplicate_number_hash_to_last_write() {
        let rows = vec![
            sample_row(1, &[0xaa], false),
            sample_row(2, &[0xbb], false),
            sample_row(1, &[0xaa], true),
        ];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2);
        let kept = deduped
            .iter()
            .find(|row| row.number == 1 && row.hash == [0xaa])
            .expect("duplicated key survives");
        assert!(kept.reorged_out, "duplicate key keeps the last write");
    }

    #[test]
    fn dedupe_keeps_same_number_with_distinct_hash() {
        let rows = vec![sample_row(1, &[0xaa], true), sample_row(1, &[0xbb], false)];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2, "distinct hashes at the same height are separate rows");
    }
}
