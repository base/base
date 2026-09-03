//! Postgres-backed system tests for shadow block reconciliation.

use std::time::Duration;

use alloy_consensus::{Block, BlockBody, Header, SignableTransaction, TxEip1559};
use alloy_primitives::{Address, Signature};
use anyhow::Result;
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_shadow_indexer_db::{
    PgConnectionParams, ShadowBlockPayload, ShadowBlockRepo, ShadowBlockRow, ShadowCanonicalRef,
    ShadowDbConfig, ShadowHash, ShadowWrite,
};
use chrono::Utc;
use reth_primitives_traits::RecoveredBlock;
use sqlx::PgPool;
use testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::time::sleep;

/// `testcontainers-modules` still defaults to Postgres 11, which predates the
/// `jsonb_path_query_array` used by migration 0004.
const POSTGRES_TAG: &str = "16-alpine";

struct TestDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDatabase {
    async fn start() -> Result<Self> {
        let container = Postgres::default().with_tag(POSTGRES_TAG).start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let pool = ShadowDbConfig {
            connection: PgConnectionParams {
                host: "127.0.0.1".to_string(),
                port,
                database: "postgres".to_string(),
                username: "postgres".to_string(),
                password: "postgres".to_string(),
            },
            max_connections: 5,
            connection_timeout: Duration::from_secs(5),
        }
        .init_pool()
        .await?;

        Ok(Self { _container: container, pool })
    }
}

struct ShadowBlockFixture {
    number: i64,
    gas_used: u64,
    base_fee_per_gas: u64,
    deposit_count: usize,
    tips: Vec<u128>,
    builder_version: String,
}

impl ShadowBlockFixture {
    fn new(number: i64) -> Self {
        Self {
            number,
            gas_used: 21_000,
            base_fee_per_gas: 100,
            deposit_count: 0,
            tips: Vec::new(),
            builder_version: format!("builder-{number}"),
        }
    }

    fn into_row(self, hash_seed: u8, canonical_hash_seed: Option<u8>) -> ShadowBlockRow {
        let number = self.number;
        let payload = self.into_payload();
        let now = Utc::now();

        ShadowBlockRow {
            number,
            hash: ShadowHash::encode(&[hash_seed; 32]),
            canonical_hash: canonical_hash_seed.map(|seed| ShadowHash::encode(&[seed; 32])),
            created_at: now,
            updated_at: now,
            payload,
        }
    }

    fn into_payload(self) -> ShadowBlockPayload {
        let mut transactions = Vec::with_capacity(self.deposit_count + self.tips.len());
        transactions.extend(
            (0..self.deposit_count).map(|_| {
                BaseTxEnvelope::from(TxDeposit { gas_limit: 21_000, ..Default::default() })
            }),
        );
        transactions.extend(self.tips.iter().enumerate().map(|(nonce, tip)| {
            BaseTxEnvelope::Eip1559(
                TxEip1559 {
                    chain_id: 1,
                    nonce: u64::try_from(nonce).expect("fixture nonce fits in u64"),
                    gas_limit: 21_000,
                    max_fee_per_gas: u128::from(self.base_fee_per_gas) + tip,
                    max_priority_fee_per_gas: *tip,
                    ..Default::default()
                }
                .into_signed(Signature::test_signature()),
            )
        }));
        let senders = vec![Address::ZERO; transactions.len()];
        let block = Block::<BaseTxEnvelope> {
            header: Header {
                gas_used: self.gas_used,
                base_fee_per_gas: Some(self.base_fee_per_gas),
                ..Default::default()
            },
            body: BlockBody { transactions, ..Default::default() },
        };

        ShadowBlockPayload {
            builder_version: self.builder_version,
            block: RecoveredBlock::new_unhashed(block, senders),
            receipts: Vec::new(),
        }
    }
}

fn reorged(rows: impl IntoIterator<Item = ShadowBlockRow>) -> Vec<ShadowWrite> {
    rows.into_iter().map(|row| ShadowWrite::Reorged(Box::new(row))).collect()
}

fn canonical(refs: impl IntoIterator<Item = ShadowCanonicalRef>) -> Vec<ShadowWrite> {
    refs.into_iter().map(ShadowWrite::Canonical).collect()
}

#[tokio::test]
async fn canonical_block_never_clears_an_established_hash() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(71).into_row(0x91, Some(0x92))])).await?;

    // A redelivered notification carries no canonical hash and must not erase the known one.
    repo.flush(&reorged([ShadowBlockFixture::new(71).into_row(0x91, None)])).await?;

    let rows = repo.list_by_number_range(71, 71).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].canonical_hash,
        Some(ShadowHash::encode(&[0x92; 32])),
        "canonical hash is monotonic"
    );

    Ok(())
}

#[tokio::test]
async fn a_later_candidate_at_a_height_does_not_inherit_the_replaced_hash() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(81).into_row(0xa1, Some(0xa2))])).await?;

    repo.flush(&reorged([ShadowBlockFixture::new(81).into_row(0xa3, None)])).await?;

    let rows = repo.list_by_number_range(81, 81).await?;
    assert_eq!(rows.len(), 1, "a height keys one row");
    assert_eq!(
        rows[0].hash,
        ShadowHash::encode(&[0xa3; 32]),
        "the new candidate replaces the old one"
    );
    assert_eq!(
        rows[0].canonical_hash, None,
        "the replaced candidate's canonical hash must not carry over to a different block"
    );

    Ok(())
}

#[tokio::test]
async fn a_canonical_ref_does_not_resolve_a_candidate_stored_after_it() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());

    let mut writes = reorged([ShadowBlockFixture::new(91).into_row(0xb1, None)]);
    writes.extend(canonical([ShadowCanonicalRef {
        number: 91,
        hash: ShadowHash::encode(&[0xb2; 32]),
    }]));
    writes.extend(reorged([ShadowBlockFixture::new(91).into_row(0xb3, None)]));
    repo.flush(&writes).await?;

    let rows = repo.list_by_number_range(91, 91).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].hash,
        ShadowHash::encode(&[0xb3; 32]),
        "the last candidate at the height is stored"
    );
    assert_eq!(
        rows[0].canonical_hash, None,
        "the ref replaced the earlier candidate and must not be pinned onto the later one"
    );

    Ok(())
}

#[tokio::test]
async fn a_replacement_candidate_does_not_inherit_the_previous_creation_time() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(95).into_row(0xd1, None)])).await?;
    let first_created_at = repo.list_by_number_range(95, 95).await?[0].created_at;

    sleep(Duration::from_millis(2)).await;
    repo.flush(&reorged([ShadowBlockFixture::new(95).into_row(0xd2, None)])).await?;

    let rows = repo.list_by_number_range(95, 95).await?;
    assert!(rows[0].created_at > first_created_at, "a different block is not the same discovery");

    sleep(Duration::from_millis(2)).await;
    let redelivered_at = rows[0].created_at;
    repo.flush(&reorged([ShadowBlockFixture::new(95).into_row(0xd2, None)])).await?;
    assert_eq!(
        repo.list_by_number_range(95, 95).await?[0].created_at,
        redelivered_at,
        "a redelivery of the same block keeps its original creation time"
    );

    Ok(())
}

#[tokio::test]
async fn unresolved_backlog_counts_rows_awaiting_a_canonical_block() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([
        ShadowBlockFixture::new(101).into_row(0xc1, None),
        ShadowBlockFixture::new(102).into_row(0xc2, Some(0xc3)),
    ]))
    .await?;

    let backlog = repo.unresolved_backlog().await?;
    assert_eq!(backlog.count, 1, "only the row without a canonical hash is outstanding");
    assert!(backlog.oldest_age_seconds >= 0.0);

    repo.flush(&canonical([ShadowCanonicalRef {
        number: 101,
        hash: ShadowHash::encode(&[0xc4; 32]),
    }]))
    .await?;

    let drained = repo.unresolved_backlog().await?;
    assert_eq!(drained.count, 0);
    assert!((drained.oldest_age_seconds - 0.0).abs() < f64::EPSILON, "no rows means no age");

    Ok(())
}

#[tokio::test]
async fn list_recent_returns_resolved_rows_newest_first_and_pages_by_before() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([
        ShadowBlockFixture::new(300).into_row(0x01, Some(0x11)),
        ShadowBlockFixture::new(301).into_row(0x02, None),
        ShadowBlockFixture::new(302).into_row(0x03, Some(0x12)),
        ShadowBlockFixture::new(303).into_row(0x04, Some(0x13)),
    ]))
    .await?;

    let page1 = repo.list_recent(2, None).await?;
    assert_eq!(
        page1.iter().map(|row| row.number).collect::<Vec<_>>(),
        [303, 302],
        "newest resolved rows first, capped by limit"
    );

    let page2 = repo.list_recent(2, Some(page1.last().expect("page1 not empty").number)).await?;
    assert_eq!(
        page2.iter().map(|row| row.number).collect::<Vec<_>>(),
        [300],
        "before excludes the cursor row and the unresolved 301 is skipped"
    );

    Ok(())
}

/// Reads the hash columns as the Snowflake ETL sees them, after the BYTEA columns are gone.
async fn stored_hashes(pool: &PgPool, number: i64) -> Result<(String, Option<String>)> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT hash, canonical_hash FROM shadow_blocks WHERE number = $1",
    )
    .bind(number)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

#[tokio::test]
async fn hashes_are_stored_as_text_the_etl_can_read() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(500).into_row(0xe1, Some(0xe2))])).await?;

    // Decoding into String is itself the assertion that the column is no longer BYTEA.
    let (hash, canonical_hash) = stored_hashes(&database.pool, 500).await?;
    assert_eq!(hash, format!("0x{}", "e1".repeat(32)));
    assert_eq!(canonical_hash, Some(format!("0x{}", "e2".repeat(32))));

    Ok(())
}

#[tokio::test]
async fn the_backlog_index_survives_dropping_the_column_its_predicate_named() -> Result<()> {
    let database = TestDatabase::start().await?;

    // `idx_shadow_blocks_unresolved` is partial on `WHERE canonical_hash IS NULL`, so dropping
    // that column in 0015 takes the index with it -- unnamed, unlogged, and with no error. The
    // query it backs runs on every retention tick, so the loss would surface only as a table
    // scan that never stops happening. 0013 rebuilds it beforehand; this is the guard that 0013
    // is still there. Asserted on existence rather than a plan, because on a table this small
    // the planner would choose a sequential scan either way.
    let rebuilt = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_indexes \
         WHERE tablename = 'shadow_blocks' AND indexname = 'idx_shadow_blocks_unresolved'",
    )
    .fetch_one(&database.pool)
    .await?;

    assert_eq!(rebuilt, 1, "the backlog index outlived the column its predicate referenced");

    Ok(())
}

#[tokio::test]
async fn an_unresolved_row_still_registers_in_the_backlog_after_the_contract() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(501).into_row(0xf1, None)])).await?;

    assert_eq!(repo.unresolved_backlog().await?.count, 1);

    repo.flush(&canonical([ShadowCanonicalRef {
        number: 501,
        hash: ShadowHash::encode(&[0xf2; 32]),
    }]))
    .await?;

    assert_eq!(repo.unresolved_backlog().await?.count, 0, "the text predicate resolves the row");
    let (_, canonical_hash) = stored_hashes(&database.pool, 501).await?;
    assert_eq!(canonical_hash, Some(format!("0x{}", "f2".repeat(32))));

    Ok(())
}

#[tokio::test]
async fn a_row_is_retrievable_by_the_hash_string_it_was_stored_under() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.flush(&reorged([ShadowBlockFixture::new(502).into_row(0xd1, Some(0xd2))])).await?;

    let found = repo
        .get_by_block_hash(&ShadowHash::encode(&[0xd1; 32]))
        .await?
        .expect("stored row is found by its hash");
    assert_eq!(found.number, 502);

    let candidates = repo.list_reorged_by_canonical(&ShadowHash::encode(&[0xd2; 32])).await?;
    assert_eq!(
        candidates.iter().map(|row| row.number).collect::<Vec<_>>(),
        [502],
        "by-canonical lookup matches on the text column"
    );

    Ok(())
}

#[tokio::test]
async fn the_database_still_rejects_a_hash_that_is_not_lowercase_0x_hex() -> Result<()> {
    let database = TestDatabase::start().await?;

    let rejected = sqlx::query(
        "INSERT INTO shadow_blocks (number, hash, created_at, payload) \
         VALUES ($1, $2, now(), '{}'::jsonb)",
    )
    .bind(503_i64)
    .bind(format!("0X{}", "E9".repeat(32)))
    .execute(&database.pool)
    .await;

    let error = rejected.expect_err("uppercase hex is not the spelling the reader looks up");
    assert!(
        error.to_string().contains("shadow_blocks_hash_format"),
        "the renamed format constraint is what rejected it: {error}"
    );

    Ok(())
}
