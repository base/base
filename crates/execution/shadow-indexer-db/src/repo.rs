use std::collections::HashMap;

use anyhow::{Context, Result};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder, query, query_as, types::Json};

use crate::{ShadowBlockCursor, ShadowBlockRow, ShadowCanonicalRef};

/// Rows written and rows resolved by a single flush.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowFlushOutcome {
    /// Rows inserted or updated.
    pub rows_written: usize,
    /// Rows that gained a canonical hash.
    pub rows_reconciled: usize,
}

/// Shadow block repository.
#[derive(Clone, Debug)]
pub struct ShadowBlockRepo {
    pool: PgPool,
}

/// Concrete block header type stored in the shadow payload.
type BlockHeader = <BaseBlock as reth_primitives_traits::Block>::Header;

/// Summary projection for a reorged-out shadow block.
///
/// Selects only the header and transactions from the JSONB `payload` (never the
/// receipts or recovered senders), so summary endpoints avoid materializing full
/// block bodies while still deriving transaction-level stats in Rust.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowSummaryRow {
    /// Persisted block number.
    pub number: i64,
    /// Raw shadow block hash.
    pub hash: Vec<u8>,
    /// Replacement (canonical) block hash after reorg.
    pub canonical_hash: Option<Vec<u8>>,
    /// Writer-stamped builder version.
    pub builder_version: String,
    /// Block header extracted from the payload.
    pub header: Json<BlockHeader>,
    /// Transaction envelopes extracted from the payload.
    pub transactions: Json<Vec<BaseTxEnvelope>>,
}

const MAX_CANDIDATES_PER_CANONICAL: i64 = 50;
const MAX_CANDIDATES_PER_BATCH: i64 = 500;

impl ShadowBlockRepo {
    /// Creates a repository.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persists reorged rows and resolves stored rows from canonical blocks.
    ///
    /// Both run in one transaction, so a reader cannot observe a row written unresolved in the
    /// same flush that resolves it.
    ///
    /// # Errors
    /// Returns an error when the transaction fails.
    pub async fn flush(
        &self,
        rows: &[ShadowBlockRow],
        canonical: &[ShadowCanonicalRef],
    ) -> Result<ShadowFlushOutcome> {
        // Five binds per row; 4,000 stays below Postgres's 65,535-parameter limit.
        const CHUNK_SIZE: usize = 4_000;

        if rows.is_empty() && canonical.is_empty() {
            return Ok(ShadowFlushOutcome::default());
        }

        let deduped = Self::dedupe_last_write_wins(rows);

        let mut tx = self.pool.begin().await.context("failed to begin shadow block transaction")?;
        let mut rows_written = 0usize;

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

            // `COALESCE` holds `canonical_hash` for a redelivered notification, which carries
            // `NULL` and must not erase a hash the backfill already established. It does not
            // apply across heights: a second reorg at one height replaces the first candidate
            // outright, hash and payload included, so the stale hash must not survive.
            query_builder.push(
                " ON CONFLICT (number) DO UPDATE SET \
                 hash = EXCLUDED.hash, \
                 canonical_hash = CASE \
                   WHEN shadow_blocks.hash = EXCLUDED.hash \
                   THEN COALESCE(EXCLUDED.canonical_hash, shadow_blocks.canonical_hash) \
                   ELSE EXCLUDED.canonical_hash END, \
                 payload = EXCLUDED.payload, \
                 updated_at = now()",
            );

            let result = query_builder
                .build()
                .execute(&mut *tx)
                .await
                .context("failed to insert shadow block batch")?;

            rows_written = rows_written.saturating_add(result.rows_affected() as usize);
        }

        let rows_reconciled = Self::resolve_canonical_hashes(&mut tx, canonical).await?;

        tx.commit().await.context("failed to commit shadow block transaction")?;

        Ok(ShadowFlushOutcome { rows_written, rows_reconciled })
    }

    async fn resolve_canonical_hashes(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        canonical: &[ShadowCanonicalRef],
    ) -> Result<usize> {
        if canonical.is_empty() {
            return Ok(0);
        }

        let (numbers, hashes) = Self::dedupe_canonical_last_write_wins(canonical);

        let result = query(
            "UPDATE shadow_blocks AS unresolved \
             SET canonical_hash = canonical.hash, updated_at = now() \
             FROM UNNEST($1::BIGINT[], $2::BYTEA[]) AS canonical(number, hash) \
             WHERE unresolved.number = canonical.number \
               AND unresolved.hash <> canonical.hash \
               AND unresolved.canonical_hash IS NULL",
        )
        .bind(&numbers)
        .bind(&hashes)
        .execute(&mut **tx)
        .await
        .context("failed to resolve canonical hashes for shadow blocks")?;

        Ok(result.rows_affected() as usize)
    }

    /// Postgres picks an arbitrary source row when several `UNNEST` entries match one target, so
    /// a height appearing twice in a flush must collapse to the last hash before binding.
    fn dedupe_canonical_last_write_wins(
        canonical: &[ShadowCanonicalRef],
    ) -> (Vec<i64>, Vec<Vec<u8>>) {
        let mut by_number: HashMap<i64, &[u8]> = HashMap::with_capacity(canonical.len());
        for entry in canonical {
            by_number.insert(entry.number, entry.hash.as_slice());
        }

        by_number.into_iter().map(|(number, hash)| (number, hash.to_vec())).unzip()
    }

    /// Postgres cannot upsert one key twice; retain its final state within each flush.
    fn dedupe_last_write_wins(rows: &[ShadowBlockRow]) -> Vec<&ShadowBlockRow> {
        let mut by_number: HashMap<i64, &ShadowBlockRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            by_number.insert(row.number, row);
        }
        by_number.into_values().collect()
    }

    /// Lists rows in an inclusive block-number range.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn list_by_number_range(&self, start: i64, end: i64) -> Result<Vec<ShadowBlockRow>> {
        let rows = query_as::<_, ShadowBlockRow>(
            "SELECT number, hash, canonical_hash, created_at, updated_at, payload \
             FROM shadow_blocks \
             WHERE number BETWEEN $1 AND $2 \
             ORDER BY number, created_at",
        )
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await
        .context("failed to list shadow blocks by number range")?;

        Ok(rows)
    }

    /// Lists reorged rows after a composite cursor.
    ///
    /// Every persisted row is reorged out, so the table needs no predicate beyond the cursor.
    /// Unresolved rows remain in the query so Rust can advance the cursor past them.
    ///
    /// # Errors
    /// Returns an error on query or payload decode failure.
    pub async fn list_reorged_since(
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
        .context("failed to list reorged shadow blocks since cursor")?;

        Ok(rows)
    }

    /// Returns the newest cursor for first-boot initialization.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn max_cursor(&self) -> Result<Option<ShadowBlockCursor>> {
        // Include unreconciled rows so first boot cannot replay them later.
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

    /// Lists reorged-out shadow candidates replaced by the canonical block with `canonical_hash`.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn list_reorged_by_canonical(
        &self,
        canonical_hash: &[u8],
    ) -> Result<Vec<ShadowSummaryRow>> {
        let rows = query_as::<_, ShadowSummaryRow>(
            "SELECT number, hash, canonical_hash, \
             payload->>'builder_version' AS builder_version, \
             payload#>'{block,block,header,header}' AS header, \
             payload#>'{block,block,body,transactions}' AS transactions \
             FROM shadow_blocks \
             WHERE canonical_hash = $1 \
             ORDER BY number DESC, created_at DESC \
             LIMIT $2",
        )
        .bind(canonical_hash)
        .bind(MAX_CANDIDATES_PER_CANONICAL)
        .fetch_all(&self.pool)
        .await
        .context("failed to list shadow candidates by canonical hash")?;

        Ok(rows)
    }

    /// Lists reorged-out shadow candidates replaced by any canonical hash in the list.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn list_reorged_by_canonicals(
        &self,
        canonical_hashes: &[Vec<u8>],
    ) -> Result<Vec<ShadowSummaryRow>> {
        if canonical_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let rows = query_as::<_, ShadowSummaryRow>(
            "SELECT number, hash, canonical_hash, \
             payload->>'builder_version' AS builder_version, \
             payload#>'{block,block,header,header}' AS header, \
             payload#>'{block,block,body,transactions}' AS transactions \
             FROM shadow_blocks \
             WHERE canonical_hash = ANY($1) \
             ORDER BY number DESC, created_at DESC \
             LIMIT $2",
        )
        .bind(canonical_hashes)
        .bind(MAX_CANDIDATES_PER_BATCH)
        .fetch_all(&self.pool)
        .await
        .context("failed to list shadow candidates by canonical hashes")?;

        Ok(rows)
    }

    /// Returns the reorged-out block with a given hash.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn get_by_block_hash(&self, hash: &[u8]) -> Result<Option<ShadowBlockRow>> {
        let row = query_as::<_, ShadowBlockRow>(
            "SELECT number, hash, canonical_hash, created_at, updated_at, payload \
             FROM shadow_blocks \
             WHERE hash = $1 \
             ORDER BY number DESC \
             LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load shadow block by hash")?;

        Ok(row)
    }

    /// Returns the summary projection for a reorged-out shadow block by its hash.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn get_summary_by_block_hash(&self, hash: &[u8]) -> Result<Option<ShadowSummaryRow>> {
        let row = query_as::<_, ShadowSummaryRow>(
            "SELECT number, hash, canonical_hash, \
             payload->>'builder_version' AS builder_version, \
             payload#>'{block,block,header,header}' AS header, \
             payload#>'{block,block,body,transactions}' AS transactions \
             FROM shadow_blocks \
             WHERE canonical_hash IS NOT NULL AND hash = $1 \
             ORDER BY number DESC \
             LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load shadow summary by hash")?;

        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use reth_primitives_traits::RecoveredBlock;

    use super::*;
    use crate::ShadowBlockPayload;

    fn sample_row(number: i64, hash: &[u8], canonical_hash: Option<Vec<u8>>) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: hash.to_vec(),
            canonical_hash,
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
    fn dedupe_collapses_duplicate_number_to_last_write() {
        let rows = vec![
            sample_row(1, &[0xaa], None),
            sample_row(2, &[0xbb], None),
            sample_row(1, &[0xaa], Some(vec![0xcc])),
        ];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 2);
        let kept = deduped.iter().find(|row| row.number == 1).expect("duplicated key survives");
        assert_eq!(kept.canonical_hash, Some(vec![0xcc]), "duplicate key keeps the last write");
    }

    #[test]
    fn dedupe_collapses_same_number_with_distinct_hash() {
        let rows = vec![sample_row(1, &[0xaa], None), sample_row(1, &[0xbb], None)];

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&rows);

        assert_eq!(deduped.len(), 1, "a height keys one row regardless of hash");
        assert_eq!(deduped[0].hash, [0xbb], "the later candidate at a height wins");
    }

    #[test]
    fn dedupe_canonical_collapses_repeated_height_to_last_hash() {
        let canonical = vec![
            ShadowCanonicalRef { number: 5, hash: vec![0x01] },
            ShadowCanonicalRef { number: 6, hash: vec![0x02] },
            ShadowCanonicalRef { number: 5, hash: vec![0x03] },
        ];

        let (numbers, hashes) = ShadowBlockRepo::dedupe_canonical_last_write_wins(&canonical);

        let mut pairs: Vec<_> = numbers.into_iter().zip(hashes).collect();
        pairs.sort_by_key(|(number, _)| *number);
        assert_eq!(pairs, vec![(5, vec![0x03]), (6, vec![0x02])]);
    }

    #[test]
    fn payload_json_paths_expose_header_and_transactions() {
        let payload = sample_row(1, &[0xaa], None).payload;
        let value = serde_json::to_value(&payload).expect("payload to json");

        let header_value = json_path(&value, &["block", "block", "header", "header"]);
        serde_json::from_value::<BlockHeader>(header_value.clone()).expect("header json roundtrip");

        let tx_value = json_path(&value, &["block", "block", "body", "transactions"]);
        serde_json::from_value::<Vec<BaseTxEnvelope>>(tx_value.clone())
            .expect("transactions json roundtrip");
    }

    fn json_path<'a>(root: &'a serde_json::Value, path: &[&str]) -> &'a serde_json::Value {
        path.iter().fold(root, |value, key| {
            value.get(*key).unwrap_or_else(|| panic!("missing json path {path:?}"))
        })
    }
}
