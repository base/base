//! Postgres-backed system tests for shadow block retention sweeps.

use std::time::Duration;

use anyhow::Result;
use base_shadow_indexer_db::{
    PgConnectionParams, SHADOW_RETENTION_LOCK_KEY, ShadowBlockPayload, ShadowBlockRepo,
    ShadowBlockRow, ShadowDbConfig, ShadowHash, ShadowRetentionRepo, ShadowWrite,
};
use chrono::Utc;
use reth_primitives_traits::RecoveredBlock;
use sqlx::{PgPool, query, query_scalar};
use testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;

const RETENTION_PERIOD: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Matches the RDS engine version the shadow builders write to. The module default is Postgres
/// 11, which predates the `MATERIALIZED` CTE hint the sweep relies on.
const POSTGRES_TAG: &str = "17-alpine";

struct TestDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDatabase {
    async fn start() -> Result<Self> {
        let container = Postgres::default().with_tag(POSTGRES_TAG).start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let connection = PgConnectionParams {
            host: "127.0.0.1".to_string(),
            port,
            database: "postgres".to_string(),
            username: "postgres".to_string(),
            password: "postgres".to_string(),
        };
        let pool = ShadowDbConfig {
            connection,
            max_connections: 5,
            connection_timeout: Duration::from_secs(5),
        }
        .init_pool()
        .await?;

        Ok(Self { _container: container, pool })
    }

    fn retention(&self) -> ShadowRetentionRepo {
        ShadowRetentionRepo::new(self.pool.clone())
    }

    async fn insert(&self, rows: Vec<ShadowBlockRow>) -> Result<()> {
        let writes: Vec<ShadowWrite> =
            rows.into_iter().map(|row| ShadowWrite::Reorged(Box::new(row))).collect();
        ShadowBlockRepo::new(self.pool.clone()).flush(&writes).await?;

        Ok(())
    }

    async fn age_all_rows(&self, age: Duration) -> Result<()> {
        let seconds = i64::try_from(age.as_secs())?;
        query("UPDATE shadow_blocks SET updated_at = now() - ($1::bigint * interval '1 second')")
            .bind(seconds)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn age_row(&self, number: i64, age: Duration) -> Result<()> {
        let seconds = i64::try_from(age.as_secs())?;
        query(
            "UPDATE shadow_blocks SET updated_at = now() - ($1::bigint * interval '1 second') \
             WHERE number = $2",
        )
        .bind(seconds)
        .bind(number)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn remaining_numbers(&self) -> Result<Vec<i64>> {
        let numbers = query_scalar("SELECT number FROM shadow_blocks ORDER BY number")
            .fetch_all(&self.pool)
            .await?;

        Ok(numbers)
    }
}

/// A distinct 32-byte hash per height; `shadow_blocks_hash_format` requires the full width.
fn block_hash_bytes(number: i64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&number.to_be_bytes());
    bytes
}

fn shadow_row(number: i64) -> ShadowBlockRow {
    let now = Utc::now();

    ShadowBlockRow {
        number,
        hash: ShadowHash::encode(&block_hash_bytes(number)),
        canonical_hash: None,
        created_at: now,
        updated_at: now,
        payload: ShadowBlockPayload {
            builder_version: "retention-test".to_string(),
            block: RecoveredBlock::default(),
            receipts: Vec::new(),
        },
    }
}

const fn days(count: u64) -> Duration {
    Duration::from_secs(count * 24 * 60 * 60)
}

#[tokio::test]
async fn deletes_only_rows_older_than_the_retention_period() -> Result<()> {
    let database = TestDatabase::start().await?;
    database.insert(vec![shadow_row(1), shadow_row(2), shadow_row(3)]).await?;

    database.age_row(1, days(31)).await?;
    database.age_row(2, days(60)).await?;

    let retention = database.retention();
    let cutoff = retention.cutoff(RETENTION_PERIOD).await?;
    let sweep = retention.sweep(cutoff).await?.expect("sweep should take the retention lock");

    assert_eq!(sweep.deleted, 2);
    assert!(!sweep.capped);
    assert_eq!(database.remaining_numbers().await?, vec![3]);

    Ok(())
}

#[tokio::test]
async fn keeps_every_row_inside_the_retention_period() -> Result<()> {
    let database = TestDatabase::start().await?;
    database.insert(vec![shadow_row(1), shadow_row(2)]).await?;

    database.age_all_rows(days(29)).await?;

    let retention = database.retention();
    let cutoff = retention.cutoff(RETENTION_PERIOD).await?;
    let sweep = retention.sweep(cutoff).await?.expect("sweep should take the retention lock");

    assert_eq!(sweep.deleted, 0);
    assert_eq!(sweep.batches, 0);
    assert_eq!(database.remaining_numbers().await?, vec![1, 2]);

    Ok(())
}

#[tokio::test]
async fn deletes_a_large_backlog_across_several_batches() -> Result<()> {
    const EXPIRED_ROWS: i64 = 2_500;

    let database = TestDatabase::start().await?;
    let rows: Vec<ShadowBlockRow> = (1..=EXPIRED_ROWS).map(shadow_row).collect();
    database.insert(rows).await?;
    database.age_all_rows(days(31)).await?;

    let retention = database.retention();
    let cutoff = retention.cutoff(RETENTION_PERIOD).await?;
    let sweep = retention.sweep(cutoff).await?.expect("sweep should take the retention lock");

    assert_eq!(sweep.deleted, u64::try_from(EXPIRED_ROWS)?);
    assert!(sweep.batches >= 2, "a backlog past one batch must span several transactions");
    assert!(database.remaining_numbers().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn yields_when_another_builder_holds_the_retention_lock() -> Result<()> {
    let database = TestDatabase::start().await?;
    database.insert(vec![shadow_row(1)]).await?;
    database.age_all_rows(days(31)).await?;

    let mut holder = database.pool.acquire().await?;
    holder.close_on_drop();
    let held: bool = query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SHADOW_RETENTION_LOCK_KEY)
        .fetch_one(&mut *holder)
        .await?;
    assert!(held, "the test must hold the lock before the sweep runs");

    let retention = database.retention();
    let cutoff = retention.cutoff(RETENTION_PERIOD).await?;

    assert!(retention.sweep(cutoff).await?.is_none());
    assert_eq!(database.remaining_numbers().await?, vec![1]);

    Ok(())
}
