use std::collections::{HashMap, hash_map::Entry};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder, query_as, types::Json};

use crate::{ShadowBlockCursor, ShadowBlockRow};

/// Shadow block repository.
#[derive(Debug)]
pub struct ShadowBlockRepo {
    pool: PgPool,
}

impl ShadowBlockRepo {
    /// Creates a repository.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts shadow block rows.
    ///
    /// # Errors
    /// Returns an error when the insert fails.
    pub async fn insert_batch(&self, rows: &[ShadowBlockRow]) -> Result<usize> {
        // Five binds per row; 4,000 stays below Postgres's 65,535-parameter limit.
        const CHUNK_SIZE: usize = 4_000;

        if rows.is_empty() {
            return Ok(0);
        }

        // Postgres cannot upsert one key twice; retain its final state within each flush.
        let deduped = Self::dedupe_last_write_wins(rows);

        let mut inserted = 0usize;

        for chunk in deduped.chunks(CHUNK_SIZE) {
            let mut query_builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
                "INSERT INTO shadow_blocks \
                 (number, hash, canonical_hash, created_at, payload) ",
            );

            query_builder.push_values(chunk, |mut row, entry| {
                row.push_bind(entry.number)
                    .push_bind(&entry.hash)
                    .push_bind(&entry.canonical_hash)
                    .push_bind(entry.created_at)
                    .push_bind(Json(&entry.payload));
            });

            query_builder.push(
                " ON CONFLICT (number) DO UPDATE SET \
                 hash = EXCLUDED.hash, \
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
        let mut by_number: HashMap<i64, &ShadowBlockRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            match by_number.entry(row.number) {
                Entry::Vacant(entry) => {
                    entry.insert(row);
                }
                Entry::Occupied(mut entry) => {
                    let current = entry.get();
                    if row.updated_at > current.updated_at
                        || (row.updated_at == current.updated_at
                            && row.hash.as_slice() > current.hash.as_slice())
                    {
                        entry.insert(row);
                    }
                }
            }
        }
        by_number.into_values().collect()
    }

    /// Lists rows in an inclusive block-number range.
    ///
    /// # Errors
    /// Returns an error when the query fails.
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

    /// Lists rows after a composite cursor.
    ///
    /// Every row qualifies because `shadow_blocks` contains only reorged-out shadow blocks.
    ///
    /// # Errors
    /// Returns an error on query or payload decode failure.
    pub async fn list_since(
        &self,
        after: &ShadowBlockCursor,
        limit: i64,
    ) -> Result<Vec<ShadowBlockRow>> {
        let rows = query_as::<_, ShadowBlockRow>(
            "SELECT number, hash, canonical_hash, created_at, updated_at, payload \
             FROM shadow_blocks \
             WHERE (updated_at, number) > ($1, $2) \
             ORDER BY updated_at, number \
             LIMIT $3",
        )
        .bind(after.updated_at)
        .bind(after.number)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to list shadow blocks since cursor")?;

        Ok(rows)
    }

    /// Returns the newest cursor for first-boot initialization.
    ///
    /// `None` is normal before the first reorg or when migration retained no rows.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn max_cursor(&self) -> Result<Option<ShadowBlockCursor>> {
        let row = query_as::<_, (DateTime<Utc>, i64)>(
            "SELECT updated_at, number FROM shadow_blocks \
             ORDER BY updated_at DESC, number DESC \
             LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("failed to load newest shadow block cursor")?;

        Ok(row.map(|(updated_at, number)| ShadowBlockCursor { updated_at, number }))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use reth_primitives_traits::RecoveredBlock;

    use super::*;
    use crate::ShadowBlockPayload;

    fn sample_row(number: i64, hash: &[u8], updated_at: DateTime<Utc>) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: hash.to_vec(),
            canonical_hash: None,
            created_at: updated_at,
            updated_at,
            payload: ShadowBlockPayload {
                builder_version: String::new(),
                block: RecoveredBlock::default(),
                receipts: Vec::new(),
            },
        }
    }

    #[test]
    fn dedupe_collapses_duplicate_number_hash_to_newest_update() {
        let older = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1);
        let newer = older + TimeDelta::seconds(1);
        let rows = vec![sample_row(1, &[0xaa], newer), sample_row(1, &[0xaa], older)];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].updated_at, newer);
    }

    #[test]
    fn dedupe_collapses_same_number_with_distinct_hash_deterministically() {
        let older_at = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1);
        let newer_at = older_at + TimeDelta::seconds(1);
        let older = sample_row(7, &[0xaa; 32], older_at);
        let newer = sample_row(7, &[0xbb; 32], newer_at);
        let forward = vec![older.clone(), newer.clone()];
        let reverse = vec![newer, older];

        for rows in [&forward, &reverse] {
            let deduped = ShadowBlockRepo::dedupe_last_write_wins(rows);

            assert_eq!(deduped.len(), 1);
            assert_eq!(deduped[0].updated_at, newer_at);
            assert_eq!(deduped[0].hash, [0xbb; 32]);
        }
    }

    #[test]
    fn dedupe_breaks_equal_update_time_ties_by_greater_hash() {
        let updated_at = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1);
        let lower = sample_row(7, &[0xaa; 32], updated_at);
        let greater = sample_row(7, &[0xbb; 32], updated_at);
        let forward = vec![lower.clone(), greater.clone()];
        let reverse = vec![greater, lower];

        for rows in [&forward, &reverse] {
            let deduped = ShadowBlockRepo::dedupe_last_write_wins(rows);

            assert_eq!(deduped.len(), 1);
            assert_eq!(deduped[0].hash, [0xbb; 32]);
        }
    }

    #[test]
    fn genesis_cursor_is_lower_than_any_real_cursor() {
        let genesis = ShadowBlockCursor::genesis();
        let real = ShadowBlockCursor {
            updated_at: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1),
            number: 1,
        };

        assert!((genesis.updated_at, genesis.number) < (real.updated_at, real.number));
    }
}
