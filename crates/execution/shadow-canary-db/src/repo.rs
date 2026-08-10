use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::{PgPool, Postgres, QueryBuilder, query_as};

use crate::{ShadowBlockRow, ShadowBlockTransactionRow};

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
        // 16 columns per row are bound. Postgres caps a single statement at 65_535
        // bind parameters, so keep chunks below 65_535 / 16 ≈ 4_095 rows.
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

            inserted = inserted.saturating_add(result.rows_affected() as usize);
        }

        Ok(inserted)
    }

    fn dedupe_last_write_wins(rows: &[ShadowBlockRow]) -> Vec<&ShadowBlockRow> {
        let mut by_key: HashMap<(i64, &str), &ShadowBlockRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            by_key.insert((row.number, row.hash.as_str()), row);
        }
        by_key.into_values().collect()
    }

    /// Insert a batch of shadow block transaction rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn insert_transactions_batch(
        &self,
        rows: &[ShadowBlockTransactionRow],
    ) -> Result<usize> {
        // 11 columns per row are bound; stay well under the 65_535 bind-parameter cap.
        const CHUNK_SIZE: usize = 4_000;

        if rows.is_empty() {
            return Ok(0);
        }

        let deduped = Self::dedupe_transactions_last_write_wins(rows);

        let mut inserted = 0usize;

        for chunk in deduped.chunks(CHUNK_SIZE) {
            let mut query_builder: QueryBuilder<'_, Postgres> = QueryBuilder::new(
                "INSERT INTO shadow_block_transactions \
                 (block_number, block_hash, tx_index, tx_hash, sender, tx_type, \
                  effective_priority_fee_per_gas, base_fee_per_gas, gas_used, reorged_out, \
                  created_at) ",
            );

            query_builder.push_values(chunk, |mut row, entry| {
                row.push_bind(entry.block_number)
                    .push_bind(&entry.block_hash)
                    .push_bind(entry.tx_index)
                    .push_bind(&entry.tx_hash)
                    .push_bind(&entry.sender)
                    .push_bind(entry.tx_type)
                    .push_bind(&entry.effective_priority_fee_per_gas)
                    .push_bind(entry.base_fee_per_gas)
                    .push_bind(entry.gas_used)
                    .push_bind(entry.reorged_out)
                    .push_bind(entry.created_at);
            });

            query_builder.push(
                " ON CONFLICT (block_hash, tx_index) DO UPDATE SET \
                 reorged_out = EXCLUDED.reorged_out",
            );

            let result = query_builder
                .build()
                .execute(&self.pool)
                .await
                .context("failed to insert shadow block transaction batch")?;

            inserted = inserted.saturating_add(result.rows_affected() as usize);
        }

        Ok(inserted)
    }

    fn dedupe_transactions_last_write_wins(
        rows: &[ShadowBlockTransactionRow],
    ) -> Vec<&ShadowBlockTransactionRow> {
        let mut by_key: HashMap<(&str, i32), &ShadowBlockTransactionRow> =
            HashMap::with_capacity(rows.len());
        for row in rows {
            by_key.insert((row.block_hash.as_str(), row.tx_index), row);
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

    use super::*;

    fn sample_row(number: i64, hash: &str, reorged_out: bool) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: hash.to_string(),
            parent_hash: "parent".to_string(),
            timestamp: 0,
            tx_count: 0,
            gas_used: 0,
            da_bytes: 0,
            state_root: "state".to_string(),
            build_latency_ms: None,
            deadline_miss: false,
            fb_count: None,
            panicked: false,
            reorged_out,
            canonical_hash: None,
            builder_version: String::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn dedupe_collapses_duplicate_number_hash_to_last_write() {
        let rows = vec![
            sample_row(1, "0xaa", false),
            sample_row(2, "0xbb", false),
            sample_row(1, "0xaa", true),
        ];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2);
        let kept = deduped
            .iter()
            .find(|row| row.number == 1 && row.hash == "0xaa")
            .expect("duplicated key survives");
        assert!(kept.reorged_out, "duplicate key keeps the last write");
    }

    #[test]
    fn dedupe_keeps_same_number_with_distinct_hash() {
        let rows = vec![sample_row(1, "0xaa", true), sample_row(1, "0xbb", false)];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2, "distinct hashes at the same height are separate rows");
    }

    fn sample_tx_row(
        block_hash: &str,
        tx_index: i32,
        reorged_out: bool,
    ) -> ShadowBlockTransactionRow {
        ShadowBlockTransactionRow {
            block_number: 1,
            block_hash: block_hash.to_string(),
            tx_index,
            tx_hash: format!("0xtx{tx_index}"),
            sender: None,
            tx_type: 2,
            effective_priority_fee_per_gas: Some("1000".to_string()),
            base_fee_per_gas: Some(7),
            gas_used: 21_000,
            reorged_out,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn dedupe_transactions_collapses_duplicate_block_hash_index_to_last_write() {
        let rows = vec![
            sample_tx_row("0xaa", 0, false),
            sample_tx_row("0xaa", 1, false),
            sample_tx_row("0xaa", 0, true),
        ];

        let deduped = ShadowBlockRepo::dedupe_transactions_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2);
        let kept = deduped
            .iter()
            .find(|row| row.block_hash == "0xaa" && row.tx_index == 0)
            .expect("duplicated key survives");
        assert!(kept.reorged_out, "duplicate key keeps the last write");
    }

    #[test]
    fn dedupe_transactions_keeps_same_index_across_distinct_blocks() {
        let rows = vec![sample_tx_row("0xaa", 0, false), sample_tx_row("0xbb", 0, false)];

        let deduped = ShadowBlockRepo::dedupe_transactions_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2, "same tx_index in different blocks are separate rows");
    }
}
