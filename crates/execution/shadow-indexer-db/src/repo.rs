use std::collections::HashMap;

use anyhow::{Context, Result};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use sqlx::{PgPool, Postgres, QueryBuilder, query, query_as, types::Json};

use crate::{ShadowBlockRow, ShadowCanonicalRef, ShadowWrite};

/// Rows written and rows resolved by a single flush.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowFlushOutcome {
    /// Rows inserted or updated.
    pub rows_written: usize,
    /// Rows that gained a canonical hash.
    pub rows_reconciled: usize,
}

/// Stored rows still waiting for the canonical block at their height.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShadowUnresolvedBacklog {
    /// Rows with no `canonical_hash`.
    pub count: i64,
    /// Age of the oldest such row, zero when there are none.
    pub oldest_age_seconds: f64,
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
    /// Shadow block hash, as `0x`-prefixed lowercase hex.
    pub hash: String,
    /// Replacement (canonical) block hash after reorg, as `0x`-prefixed lowercase hex.
    pub canonical_hash: Option<String>,
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

    /// Applies writes in order, in one transaction.
    ///
    /// Consecutive writes of the same kind collapse into a single statement, but the runs
    /// themselves execute in the order the `ExEx` produced them. Partitioning the stream by kind
    /// instead would let a canonical ref resolve a candidate that had not yet been stored when the
    /// ref was emitted, pinning one block's replacement hash onto a different block.
    ///
    /// One transaction also keeps a reader from observing a row written unresolved in the same
    /// flush that resolves it.
    ///
    /// # Errors
    /// Returns an error when the transaction fails.
    pub async fn flush(&self, writes: &[ShadowWrite]) -> Result<ShadowFlushOutcome> {
        if writes.is_empty() {
            return Ok(ShadowFlushOutcome::default());
        }

        let mut tx = self.pool.begin().await.context("failed to begin shadow block transaction")?;
        let mut outcome = ShadowFlushOutcome::default();

        let mut rows: Vec<&ShadowBlockRow> = Vec::new();
        let mut canonical: Vec<&ShadowCanonicalRef> = Vec::new();

        for write in writes {
            match write {
                ShadowWrite::Reorged(row) => {
                    outcome.rows_reconciled +=
                        Self::resolve_canonical_hashes(&mut tx, &canonical).await?;
                    canonical.clear();
                    rows.push(row);
                }
                ShadowWrite::Canonical(entry) => {
                    outcome.rows_written += Self::insert_rows(&mut tx, &rows).await?;
                    rows.clear();
                    canonical.push(entry);
                }
            }
        }

        outcome.rows_written += Self::insert_rows(&mut tx, &rows).await?;
        outcome.rows_reconciled += Self::resolve_canonical_hashes(&mut tx, &canonical).await?;

        tx.commit().await.context("failed to commit shadow block transaction")?;

        Ok(outcome)
    }

    async fn insert_rows(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        rows: &[&ShadowBlockRow],
    ) -> Result<usize> {
        // Five binds per row; 4,000 stays below Postgres's 65,535-parameter limit.
        const CHUNK_SIZE: usize = 4_000;

        let deduped = Self::dedupe_last_write_wins(rows);
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

            // A new candidate hash lands wholesale. A same-hash conflict only reaches DO UPDATE to
            // carry a `canonical_hash`, so `payload` and `created_at` are held at their stored
            // values: reassigning `payload` to itself keeps the existing TOAST pointer, avoiding a
            // rewrite (and full-page WAL) of the ~176KB JSONB just to stamp a hash. The WHERE drops
            // redeliveries that change neither hash nor add a canonical hash.
            query_builder.push(
                " ON CONFLICT (number) DO UPDATE SET \
                 hash = EXCLUDED.hash, \
                 canonical_hash = EXCLUDED.canonical_hash, \
                 created_at = CASE WHEN shadow_blocks.hash <> EXCLUDED.hash \
                     THEN EXCLUDED.created_at ELSE shadow_blocks.created_at END, \
                 payload = CASE WHEN shadow_blocks.hash <> EXCLUDED.hash \
                     THEN EXCLUDED.payload ELSE shadow_blocks.payload END, \
                 updated_at = now() \
                 WHERE shadow_blocks.hash <> EXCLUDED.hash \
                    OR EXCLUDED.canonical_hash IS NOT NULL",
            );

            let result = query_builder
                .build()
                .execute(&mut **tx)
                .await
                .context("failed to insert shadow block batch")?;

            rows_written = rows_written.saturating_add(result.rows_affected() as usize);
        }

        Ok(rows_written)
    }

    async fn resolve_canonical_hashes(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        canonical: &[&ShadowCanonicalRef],
    ) -> Result<usize> {
        if canonical.is_empty() {
            return Ok(0);
        }

        let (numbers, hashes) = Self::dedupe_canonical_last_write_wins(canonical);

        let result = query(
            "UPDATE shadow_blocks AS unresolved \
             SET canonical_hash = canonical.hash, updated_at = now() \
             FROM UNNEST($1::BIGINT[], $2::TEXT[]) AS canonical(number, hash) \
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
        canonical: &[&ShadowCanonicalRef],
    ) -> (Vec<i64>, Vec<String>) {
        let mut by_number: HashMap<i64, &str> = HashMap::with_capacity(canonical.len());
        for entry in canonical {
            by_number.insert(entry.number, entry.hash.as_str());
        }

        by_number.into_iter().map(|(number, hash)| (number, hash.to_owned())).unzip()
    }

    /// Postgres cannot upsert one key twice; retain its final state within each run.
    fn dedupe_last_write_wins<'a>(rows: &[&'a ShadowBlockRow]) -> Vec<&'a ShadowBlockRow> {
        let mut by_number: HashMap<i64, &ShadowBlockRow> = HashMap::with_capacity(rows.len());
        for row in rows {
            by_number.insert(row.number, row);
        }
        by_number.into_values().collect()
    }

    /// Counts rows the indexer has not yet resolved, and dates the oldest.
    ///
    /// A row is never emitted until it gains a canonical hash, so a backlog that stops draining is
    /// the only outward sign of rows nothing will revisit.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn unresolved_backlog(&self) -> Result<ShadowUnresolvedBacklog> {
        // Aged against the database clock that stamped `created_at`, not the reader's.
        let (count, oldest_age_seconds) = query_as::<_, (i64, f64)>(
            "SELECT COUNT(*), \
             COALESCE(EXTRACT(EPOCH FROM (now() - MIN(created_at))), 0)::DOUBLE PRECISION \
             FROM shadow_blocks WHERE canonical_hash IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .context("failed to count unresolved shadow blocks")?;

        Ok(ShadowUnresolvedBacklog { count, oldest_age_seconds })
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

    /// Lists the most recent resolved shadow blocks, newest first.
    ///
    /// Only rows that have gained a canonical hash are returned, matching the
    /// single-block summary endpoint: an unresolved row is not yet a confirmed
    /// shadow replacement. `before` excludes rows at or above that block number,
    /// so the caller pages backwards by passing the last number it saw.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn list_recent(
        &self,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Vec<ShadowSummaryRow>> {
        let rows = query_as::<_, ShadowSummaryRow>(
            "SELECT number, hash, canonical_hash, \
             payload->>'builder_version' AS builder_version, \
             payload#>'{block,block,header,header}' AS header, \
             payload#>'{block,block,body,transactions}' AS transactions \
             FROM shadow_blocks \
             WHERE canonical_hash IS NOT NULL \
               AND ($1::BIGINT IS NULL OR number < $1) \
             ORDER BY number DESC \
             LIMIT $2",
        )
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to list recent shadow blocks")?;

        Ok(rows)
    }

    /// Lists reorged-out shadow candidates replaced by the canonical block with `canonical_hash`.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub async fn list_reorged_by_canonical(
        &self,
        canonical_hash: &str,
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
        canonical_hashes: &[String],
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
    pub async fn get_by_block_hash(&self, hash: &str) -> Result<Option<ShadowBlockRow>> {
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
    pub async fn get_summary_by_block_hash(&self, hash: &str) -> Result<Option<ShadowSummaryRow>> {
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
    use chrono::Utc;
    use reth_primitives_traits::RecoveredBlock;

    use super::*;
    use crate::ShadowBlockPayload;

    fn sample_row(number: i64, hash: &str, canonical_hash: Option<String>) -> ShadowBlockRow {
        ShadowBlockRow {
            number,
            hash: hash.to_owned(),
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
        let rows = [
            sample_row(1, "0xaa", None),
            sample_row(2, "0xbb", None),
            sample_row(1, "0xaa", Some("0xcc".to_owned())),
        ];
        let borrowed: Vec<&ShadowBlockRow> = rows.iter().collect();

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&borrowed);

        assert_eq!(deduped.len(), 2);
        let kept = deduped.iter().find(|row| row.number == 1).expect("duplicated key survives");
        assert_eq!(
            kept.canonical_hash,
            Some("0xcc".to_owned()),
            "duplicate key keeps the last write"
        );
    }

    #[test]
    fn dedupe_collapses_same_number_with_distinct_hash() {
        let rows = [sample_row(1, "0xaa", None), sample_row(1, "0xbb", None)];
        let borrowed: Vec<&ShadowBlockRow> = rows.iter().collect();

        let deduped = ShadowBlockRepo::dedupe_last_write_wins(&borrowed);

        assert_eq!(deduped.len(), 1, "a height keys one row regardless of hash");
        assert_eq!(deduped[0].hash, "0xbb", "the later candidate at a height wins");
    }

    #[test]
    fn dedupe_canonical_collapses_repeated_height_to_last_hash() {
        let entries = [
            ShadowCanonicalRef { number: 5, hash: "0x01".to_owned() },
            ShadowCanonicalRef { number: 6, hash: "0x02".to_owned() },
            ShadowCanonicalRef { number: 5, hash: "0x03".to_owned() },
        ];
        let canonical: Vec<&ShadowCanonicalRef> = entries.iter().collect();

        let (numbers, hashes) = ShadowBlockRepo::dedupe_canonical_last_write_wins(&canonical);

        let mut pairs: Vec<_> = numbers.into_iter().zip(hashes).collect();
        pairs.sort_by_key(|(number, _)| *number);
        assert_eq!(pairs, vec![(5, "0x03".to_owned()), (6, "0x02".to_owned())]);
    }

    #[test]
    fn payload_json_paths_expose_header_and_transactions() {
        let payload = sample_row(1, "0xaa", None).payload;
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
