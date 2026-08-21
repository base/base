//! Postgres-backed tests for `ShadowMetricsReader` bootstrap.
//!
//! While the indexer persisted a row for every committed canonical block, `shadow_blocks` was
//! never empty and `ShadowBlockRepo::max_cursor` returning `None` was nearly unreachable. Now
//! that only reorged-out blocks are written, a chain that has not reorged leaves the table
//! empty and the genesis-cursor branch in `ShadowMetricsReader::new` is a normal first-boot
//! outcome. These tests cover that branch alongside the resume-from-`max_cursor` branch;
//! resuming from a persisted cursor is covered in `shadow_metrics_reader.rs`.

use std::time::Duration;

use anyhow::{Result, ensure};
use base_shadow_indexer_db::{
    ShadowBlockCursor, ShadowBlockPayload, ShadowBlockRepo, ShadowBlockRow, ShadowDbConfig,
    ShadowMetricsCursorRepo,
};
use base_shadow_metrics::{ShadowMetricsReader, ShadowMetricsReaderConfig, ShadowMetricsStore};
use chrono::Utc;
use reth_primitives_traits::RecoveredBlock;
use sqlx::PgPool;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;

const TEST_MAX_ROWS_PER_POLL: u32 = 100;
const TEST_POLL_INTERVAL: Duration = Duration::from_secs(1);

struct TestDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDatabase {
    async fn start() -> Result<Self> {
        let container = Postgres::default().start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let pool =
            ShadowDbConfig { url, max_connections: 5, connection_timeout: Duration::from_secs(5) }
                .init_pool()
                .await?;
        Ok(Self { _container: container, pool })
    }
}

fn sample_payload(builder_version: &str) -> ShadowBlockPayload {
    ShadowBlockPayload {
        builder_version: builder_version.to_string(),
        block: RecoveredBlock::default(),
        receipts: Vec::new(),
    }
}

fn sample_row(number: i64, hash_seed: u8, canonical_hash_seed: Option<u8>) -> ShadowBlockRow {
    let now = Utc::now();
    ShadowBlockRow {
        number,
        hash: vec![hash_seed; 32],
        reorged_out: true,
        canonical_hash: canonical_hash_seed.map(|seed| vec![seed; 32]),
        created_at: now,
        updated_at: now,
        payload: sample_payload("test-builder"),
    }
}

const fn reader_config() -> ShadowMetricsReaderConfig {
    ShadowMetricsReaderConfig {
        poll_interval: TEST_POLL_INTERVAL,
        max_rows_per_poll: TEST_MAX_ROWS_PER_POLL,
    }
}

#[tokio::test]
async fn boots_at_genesis_against_empty_table_and_persists_cursor() -> Result<()> {
    let database = TestDatabase::start().await?;
    let pool = database.pool.clone();

    let mut reader =
        ShadowMetricsReader::new(ShadowMetricsStore::new(pool.clone()), reader_config()).await?;

    // Round-trip through the database, not the in-memory `reader`: `reader.rs:88-89` persists
    // the bootstrap cursor immediately "so restart before first batch cannot skip intervening
    // rows", and this assertion is checking exactly that promise.
    let persisted = ShadowMetricsCursorRepo::new(pool.clone())
        .load()
        .await?
        .expect("reader bootstrap persists a cursor even against an empty table");
    ensure!(
        persisted == ShadowBlockCursor::genesis(),
        "expected persisted cursor to equal genesis, got {persisted:?}"
    );

    let emitted = reader.poll_once().await?;
    ensure!(emitted.is_empty(), "expected no stats from an empty table, got {emitted:?}");

    Ok(())
}

#[tokio::test]
async fn genesis_boot_does_not_skip_rows_that_arrive_afterwards() -> Result<()> {
    let database = TestDatabase::start().await?;
    let pool = database.pool.clone();

    let mut reader =
        ShadowMetricsReader::new(ShadowMetricsStore::new(pool.clone()), reader_config()).await?;

    ShadowBlockRepo::new(pool.clone()).insert_batch(&[sample_row(1, 0x11, Some(0x91))]).await?;

    let emitted = reader.poll_once().await?;
    ensure!(
        emitted.len() == 1,
        "a genesis cursor must not swallow rows written after bootstrap, got {} stats",
        emitted.len()
    );
    ensure!(emitted[0].number == 1, "unexpected emitted block number: {}", emitted[0].number);

    Ok(())
}

#[tokio::test]
async fn non_empty_table_on_first_boot_skips_the_backlog() -> Result<()> {
    let database = TestDatabase::start().await?;
    let pool = database.pool.clone();

    let seeded_numbers = [10_i64, 20, 30];
    ShadowBlockRepo::new(pool.clone())
        .insert_batch(&[
            sample_row(seeded_numbers[0], 0x21, Some(0xa1)),
            sample_row(seeded_numbers[1], 0x22, Some(0xa2)),
            sample_row(seeded_numbers[2], 0x23, Some(0xa3)),
        ])
        .await?;

    let mut reader =
        ShadowMetricsReader::new(ShadowMetricsStore::new(pool.clone()), reader_config()).await?;

    let persisted = ShadowMetricsCursorRepo::new(pool.clone())
        .load()
        .await?
        .expect("reader bootstrap persists a cursor when resuming from max_cursor");
    ensure!(
        persisted.number == *seeded_numbers.iter().max().expect("non-empty seed"),
        "expected bootstrap cursor to resume from the newest seeded row, got {persisted:?}"
    );

    let emitted = reader.poll_once().await?;
    ensure!(
        emitted.is_empty(),
        "a first boot against a non-empty table must skip the backlog, got {} stats",
        emitted.len()
    );

    Ok(())
}
