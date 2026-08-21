//! Postgres-backed tests for migrating legacy shadow indexer data.

use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{Result, ensure};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_0001: &str =
    include_str!("../../../crates/execution/shadow-indexer-db/migrations/0001_init.sql");
const MIGRATION_0002: &str =
    include_str!("../../../crates/execution/shadow-indexer-db/migrations/0002_updated_at.sql");
const MIGRATION_0003: &str =
    include_str!("../../../crates/execution/shadow-indexer-db/migrations/0003_metrics_cursor.sql");
const MIGRATION_0004: &str =
    include_str!("../../../crates/execution/shadow-indexer-db/migrations/0004_number_only_key.sql");

async fn connect_legacy_pool(url: &str) -> Result<PgPool> {
    // Connect directly instead of using ShadowDbConfig's pool initializer: it would apply 0004
    // before legacy data is seeded and defeat this migration test.
    Ok(PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?)
}

async fn apply_legacy_migrations(pool: &PgPool) -> Result<()> {
    for migration in [MIGRATION_0001, MIGRATION_0002, MIGRATION_0003] {
        sqlx::raw_sql(migration).execute(pool).await?;
    }
    Ok(())
}

#[tokio::test]
async fn migrates_populated_legacy_shadow_blocks() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = connect_legacy_pool(&url).await?;

    apply_legacy_migrations(&pool).await?;
    sqlx::raw_sql(
        "INSERT INTO shadow_blocks \
         (number, hash, reorged_out, canonical_hash, created_at, updated_at, payload) \
         VALUES \
         (100, decode(repeat('bb', 32), 'hex'), true, \
          decode(repeat('c2', 32), 'hex'), \
          '2024-01-01T00:00:00.100002Z'::timestamptz, \
          '2024-02-02T01:02:03.654321Z'::timestamptz, '{}'::jsonb), \
         (100, decode(repeat('aa', 32), 'hex'), true, \
          decode(repeat('c1', 32), 'hex'), \
          '2024-01-01T00:00:00.100001Z'::timestamptz, \
          '2024-02-01T01:02:03.123456Z'::timestamptz, '{}'::jsonb), \
         (101, decode(repeat('cc', 32), 'hex'), false, NULL, \
          '2024-01-01T00:00:00.100003Z'::timestamptz, \
          '2024-02-01T01:02:03.123456Z'::timestamptz, '{}'::jsonb), \
         (102, decode(repeat('dd', 32), 'hex'), true, NULL, \
          '2024-01-01T00:00:00.100004Z'::timestamptz, \
          '2024-02-01T01:02:03.123456Z'::timestamptz, '{}'::jsonb), \
         (103, decode(repeat('ee', 32), 'hex'), true, \
          decode(repeat('c3', 32), 'hex'), \
          '2024-01-01T00:00:00.100005Z'::timestamptz, \
          '2024-02-01T01:02:03.123456Z'::timestamptz, '{}'::jsonb), \
         (103, decode(repeat('ff', 32), 'hex'), false, NULL, \
          '2024-01-01T00:00:00.100006Z'::timestamptz, \
          '2024-02-02T01:02:03.654321Z'::timestamptz, '{}'::jsonb)",
    )
    .execute(&pool)
    .await?;

    sqlx::raw_sql(MIGRATION_0004).execute(&pool).await?;

    let numbers: Vec<i64> = sqlx::query_scalar("SELECT number FROM shadow_blocks ORDER BY number")
        .fetch_all(&pool)
        .await?;
    ensure!(numbers == [100, 102, 103], "unexpected surviving block numbers: {numbers:?}");

    let row_100: (Vec<u8>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT hash, canonical_hash FROM shadow_blocks WHERE number = 100")
            .fetch_one(&pool)
            .await?;
    ensure!(row_100.0 == vec![0xbb; 32], "newer row did not win for block 100");
    ensure!(row_100.1 == Some(vec![0xc2; 32]), "canonical hash was not preserved for block 100");

    let row_103: (Vec<u8>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT hash, canonical_hash FROM shadow_blocks WHERE number = 103")
            .fetch_one(&pool)
            .await?;
    ensure!(
        row_103 == (vec![0xee; 32], Some(vec![0xc3; 32])),
        "canonical block incorrectly won dedupe for block 103: {row_103:?}"
    );

    let timestamps: Vec<(i64, DateTime<Utc>, DateTime<Utc>)> =
        sqlx::query_as("SELECT number, created_at, updated_at FROM shadow_blocks ORDER BY number")
            .fetch_all(&pool)
            .await?;
    let expected_timestamps = vec![
        (
            100,
            DateTime::parse_from_rfc3339("2024-01-01T00:00:00.100002Z")?.with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2024-02-02T01:02:03.654321Z")?.with_timezone(&Utc),
        ),
        (
            102,
            DateTime::parse_from_rfc3339("2024-01-01T00:00:00.100004Z")?.with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2024-02-01T01:02:03.123456Z")?.with_timezone(&Utc),
        ),
        (
            103,
            DateTime::parse_from_rfc3339("2024-01-01T00:00:00.100005Z")?.with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2024-02-01T01:02:03.123456Z")?.with_timezone(&Utc),
        ),
    ];
    ensure!(
        timestamps == expected_timestamps,
        "migration changed seeded timestamps: actual={timestamps:?}, expected={expected_timestamps:?}"
    );

    let canonical_hash_102: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT canonical_hash FROM shadow_blocks WHERE number = 102")
            .fetch_one(&pool)
            .await?;
    ensure!(canonical_hash_102.is_none(), "block 102 lost its unwind discriminator");

    let legacy_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shadow_blocks_legacy").fetch_one(&pool).await?;
    ensure!(legacy_count == 6, "legacy table row count changed: {legacy_count}");

    let temporary_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.shadow_blocks_new')::text")
            .fetch_one(&pool)
            .await?;
    ensure!(temporary_table.is_none(), "temporary migration table remains: {temporary_table:?}");

    let last_hash_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'public' \
           AND table_name = 'shadow_metrics_cursor' \
           AND column_name = 'last_hash'",
    )
    .fetch_one(&pool)
    .await?;
    ensure!(last_hash_columns == 0, "shadow_metrics_cursor still has last_hash");

    let primary_key_columns: Option<String> = sqlx::query_scalar(
        "SELECT string_agg(kcu.column_name, ',' ORDER BY kcu.ordinal_position) \
         FROM information_schema.table_constraints AS tc \
         JOIN information_schema.key_column_usage AS kcu \
           ON tc.constraint_catalog = kcu.constraint_catalog \
          AND tc.constraint_schema = kcu.constraint_schema \
          AND tc.constraint_name = kcu.constraint_name \
         WHERE tc.table_schema = 'public' \
           AND tc.table_name = 'shadow_blocks' \
           AND tc.constraint_type = 'PRIMARY KEY'",
    )
    .fetch_one(&pool)
    .await?;
    ensure!(
        primary_key_columns.as_deref() == Some("number"),
        "unexpected shadow_blocks primary key: {primary_key_columns:?}"
    );

    Ok(())
}

#[tokio::test]
async fn migrates_empty_legacy_shadow_blocks() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = connect_legacy_pool(&url).await?;

    apply_legacy_migrations(&pool).await?;
    sqlx::raw_sql(MIGRATION_0004).execute(&pool).await?;

    let shadow_blocks: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.shadow_blocks')::text")
            .fetch_one(&pool)
            .await?;
    ensure!(shadow_blocks.as_deref() == Some("shadow_blocks"), "shadow_blocks does not exist");
    let shadow_blocks_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shadow_blocks").fetch_one(&pool).await?;
    ensure!(shadow_blocks_count == 0, "shadow_blocks is not empty: {shadow_blocks_count}");

    let legacy_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.shadow_blocks_legacy')::text")
            .fetch_one(&pool)
            .await?;
    ensure!(
        legacy_table.as_deref() == Some("shadow_blocks_legacy"),
        "shadow_blocks_legacy does not exist"
    );
    let legacy_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shadow_blocks_legacy").fetch_one(&pool).await?;
    ensure!(legacy_count == 0, "shadow_blocks_legacy is not empty: {legacy_count}");

    Ok(())
}
