//! Postgres integration tests for transaction event ingest.
//!
//! Run with:
//!
//! ```bash
//! DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
//!   cargo test -p audit-archiver-lib --test postgres_transaction_events -- --ignored
//! ```

use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use audit_archiver_lib::{
    MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE, PgTransactionEventSink, RejectedTransactionEventQuery,
    TransactionEventRetentionConfig, TransactionEventSchemaReadinessError, TransactionEventSink,
};
use base_observability_events::TransactionEvent;
use chrono::Utc;
use serde_json::json;
use sqlx::{Executor, PgPool, migrate::Migrator, postgres::PgPoolOptions};
use testcontainers::{ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;

/// Production RDS is Postgres 17. `testcontainers-modules` still defaults to
/// Postgres 11, which lacks the partitioning features the schema relies on.
const POSTGRES_TAG: &str = "17-alpine";

/// Hot class partitions for one UTC day, as created by migration 005.
const HOT_PARTITIONS_SQL: &str = "SELECT c.relname::text FROM pg_inherits i \
     JOIN pg_class c ON c.oid = i.inhrelid \
     WHERE i.inhparent = 'transaction_events_hot'::regclass \
     ORDER BY c.relname";

struct PostgresHarness {
    port: u16,
    database_url: String,
    _container: testcontainers::ContainerAsync<Postgres>,
}

impl PostgresHarness {
    async fn new() -> anyhow::Result<Self> {
        let container = Postgres::default().with_tag(POSTGRES_TAG).start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        Ok(Self { port, database_url, _container: container })
    }

    fn url_for(&self, user: &str, password: &str) -> String {
        format!("postgres://{user}:{password}@127.0.0.1:{}/postgres", self.port)
    }
}

fn unique_event_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("postgres-integration-{nanos}")
}

fn event(event_id: &str) -> TransactionEvent {
    event_with_type(event_id, "BUILDER_ACCEPTED")
}

fn event_with_type(event_id: &str, event_type: &str) -> TransactionEvent {
    serde_json::from_value(json!({
        "schema_version": "transaction-event/v1",
        "event_id": event_id,
        "event_time": Utc::now(),
        "producer": "base-builder",
        "event_type": event_type,
        "network": "base-mainnet",
        "tx_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
        "block_hash": null,
        "block_number": 123,
        "payload_id": "payload-1",
        "request_id": "request-1",
        "data": {
            "position": 1
        }
    }))
    .unwrap()
}

fn utc_today_at(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
    Utc::now().date_naive().and_hms_opt(hour, minute, second).unwrap().and_utc()
}

async fn cleanup(pool: &PgPool, event_id: &str) {
    let _ = pool
        .execute(sqlx::query("DELETE FROM transaction_events WHERE event_id = $1").bind(event_id))
        .await;
}

async fn event_ids_like(pool: &PgPool, prefix: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT event_id FROM transaction_events WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{prefix}-%"))
    .fetch_all(pool)
    .await?)
}

async fn hot_partitions(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(HOT_PARTITIONS_SQL).fetch_all(pool).await?)
}

/// Writes the retired pre-partition migrations up to and including
/// `last_version` into a temporary directory, so tests can build a database
/// at an older schema.
fn legacy_migrations_through(last_version: i64) -> anyhow::Result<PathBuf> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy_migrations");
    let target = std::env::temp_dir().join(format!("audit-migrations-{}", unique_event_id()));
    std::fs::create_dir_all(&target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let version: i64 = name.split('_').next().unwrap_or_default().parse()?;
        if version <= last_version {
            std::fs::copy(entry.path(), target.join(name))?;
        }
    }
    Ok(target)
}

#[tokio::test]
async fn transaction_events_ready_without_postgres_sink() {
    PgTransactionEventSink::check_optional_schema_ready(None).await.unwrap();
}

#[tokio::test]
async fn transaction_events_unready_before_required_migration() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;

    let err = sink.check_schema_ready().await.unwrap_err();
    assert!(matches!(err, TransactionEventSchemaReadinessError::MigrationTableMissing));
    assert!(err.to_string().contains("audit-archiver migrate up"));

    Ok(())
}

#[tokio::test]
async fn transaction_events_unready_when_migration_version_is_missing() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    pool.execute(sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT, success BOOLEAN)"))
        .await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let expected_version =
        PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?;

    let err = sink.check_schema_ready().await.unwrap_err();
    assert!(matches!(
        err,
        TransactionEventSchemaReadinessError::RequiredMigrationMissing {
            required_version
        } if required_version == expected_version
    ));
    assert!(err.to_string().contains("005_transaction_events_partitioned.sql"));

    Ok(())
}

#[test]
fn transaction_events_migration_version_matches_sqlx_migration_metadata() -> anyhow::Result<()> {
    assert_eq!(
        PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?,
        5,
        "005_transaction_events_partitioned.sql should resolve to sqlx migration version 5"
    );
    Ok(())
}

#[tokio::test]
async fn transaction_events_unready_on_pre_partition_schema() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    Migrator::new(legacy_migrations_through(4)?).await?.run(&pool).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;

    let err = sink.check_schema_ready().await.unwrap_err();
    assert!(
        matches!(err, TransactionEventSchemaReadinessError::RequiredMigrationMissing { .. }),
        "new pods must not go ready against the unpartitioned table: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn transaction_events_ready_after_required_migration() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;

    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;

    sink.check_schema_ready().await?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let applied: (bool,) =
        sqlx::query_as("SELECT success FROM _sqlx_migrations WHERE version = $1")
            .bind(PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?)
            .fetch_one(&pool)
            .await?;
    assert!(applied.0);

    Ok(())
}

#[tokio::test]
async fn postgres_partition_migration_discards_pre_partition_rows() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    Migrator::new(legacy_migrations_through(4)?).await?.run(&pool).await?;
    sqlx::query(
        "INSERT INTO transaction_events \
         (event_id, schema_version, event_time, producer, event_type, network, data) \
         VALUES ('legacy-row', 'transaction-event/v1', now(), 'base-builder', \
                 'BUILDER_ACCEPTED', 'base-mainnet', '{}'::jsonb)",
    )
    .execute(&pool)
    .await?;

    PgTransactionEventSink::migrate(&harness.database_url).await?;

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transaction_events").fetch_one(&pool).await?;
    assert_eq!(rows, 0, "migration 005 replaces the table instead of copying rows");
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(versions, vec![1, 2, 3, 4, 5], "migration history is preserved");

    Ok(())
}

#[tokio::test]
async fn postgres_fresh_database_only_runs_the_partitioned_baseline() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;

    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(versions, vec![5]);

    Ok(())
}

/// Mainnet shape: 003 dropped the rejected index and recorded, then 004's
/// concurrent rebuild was killed before recording. Migrating must not run 004
/// against the old table, and the invalid index must not survive.
#[tokio::test]
async fn postgres_migrates_past_an_unrecorded_004_with_an_invalid_index() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    Migrator::new(legacy_migrations_through(3)?).await?.run(&pool).await?;
    sqlx::query(
        "INSERT INTO transaction_events \
         (event_id, schema_version, event_time, producer, event_type, network, data) \
         SELECT 'legacy-' || n, 'transaction-event/v1', now(), 'base-builder', \
                'BUILDER_REJECTED', 'base-mainnet', '{}'::jsonb \
         FROM generate_series(1, 2) AS n",
    )
    .execute(&pool)
    .await?;
    // A failed concurrent build leaves an invalid index behind under the name
    // 004 would skip with IF NOT EXISTS.
    let failed = sqlx::query(
        "CREATE UNIQUE INDEX CONCURRENTLY transaction_events_rejected_event_time_idx \
         ON transaction_events (event_type)",
    )
    .execute(&pool)
    .await;
    assert!(failed.is_err(), "duplicate event types make the build fail");
    let invalid: bool = sqlx::query_scalar(
        "SELECT NOT indisvalid FROM pg_index \
         WHERE indexrelid = 'transaction_events_rejected_event_time_idx'::regclass",
    )
    .fetch_one(&pool)
    .await?;
    assert!(invalid);

    PgTransactionEventSink::migrate(&harness.database_url).await?;

    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(versions, vec![1, 2, 3, 5], "004 is retired, not run");
    let (valid, partitioned): (bool, bool) = sqlx::query_as(
        "SELECT i.indisvalid, c.relkind = 'I' FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE i.indexrelid = 'transaction_events_rejected_event_time_idx'::regclass",
    )
    .fetch_one(&pool)
    .await?;
    assert!(valid && partitioned, "the index is 005's partitioned index, not the leftover");
    PgTransactionEventSink::connect(&harness.database_url, 1).await?.check_schema_ready().await?;

    Ok(())
}

#[tokio::test]
async fn postgres_migrate_rejects_unknown_applied_migrations() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES (99, 'from a newer binary', true, '\\x00'::bytea, 0)",
    )
    .execute(&pool)
    .await?;

    let err = PgTransactionEventSink::migrate(&harness.database_url).await.unwrap_err();
    assert!(err.to_string().contains("[99]"), "{err}");

    Ok(())
}

#[tokio::test]
async fn postgres_schema_is_partitioned_by_class_then_day() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;

    let class_partitions: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i \
         JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'transaction_events'::regclass \
         ORDER BY c.relname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        class_partitions,
        vec!["transaction_events_cold", "transaction_events_hot", "transaction_events_warm"]
    );

    // Migration 005 seeds each default retention window (hot is 3 days)
    // through three days ahead.
    let today = Utc::now().date_naive();
    let expected: Vec<String> = (-3..=3)
        .map(|offset| {
            format!(
                "transaction_events_hot_{}",
                (today + chrono::Duration::days(offset)).format("%Y%m%d")
            )
        })
        .collect();
    assert_eq!(hot_partitions(&pool).await?, expected);

    let dropped_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE schemaname = 'public' \
           AND indexname IN ( \
             'transaction_events_payload_id_event_time_idx', \
             'transaction_events_producer_event_type_event_time_idx', \
             'transaction_events_event_type_ingested_at_idx' \
           )",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(dropped_indexes, 0, "unused indexes are not recreated");

    Ok(())
}

#[tokio::test]
async fn postgres_rejected_index_includes_builder_expired() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;

    let indexdef: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = 'public' \
           AND indexname = 'transaction_events_rejected_event_time_idx'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        indexdef.contains("BUILDER_EXPIRED"),
        "rejected index should include BUILDER_EXPIRED: {indexdef}"
    );

    Ok(())
}

#[tokio::test]
async fn postgres_sink_chunks_large_direct_inserts() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;

    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let event_count = MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE + 1;
    let event_prefix = unique_event_id();
    let events =
        (0..event_count).map(|index| event(&format!("{event_prefix}-{index}"))).collect::<Vec<_>>();

    let outcome = sink.insert_events(&events).await?;

    assert_eq!(outcome.inserted_event_ids.len(), event_count);
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM transaction_events WHERE event_id LIKE $1")
            .bind(format!("{event_prefix}-%"))
            .fetch_one(&pool)
            .await?;
    assert_eq!(count.0, i64::try_from(event_count)?);

    Ok(())
}

#[tokio::test]
async fn postgres_sink_dedupes_retried_events_and_routes_by_class() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();
    let events = [
        event(&format!("{event_prefix}-hot")),
        event_with_type(&format!("{event_prefix}-warm"), "INGRESS_RECEIVED"),
        event_with_type(&format!("{event_prefix}-cold"), "SIMULATION_FAILED"),
    ];

    let first = sink.insert_events(&events).await?;
    assert_eq!(first.inserted_event_ids.len(), 3);
    let retry = sink.insert_events(&events).await?;
    assert!(retry.inserted_event_ids.is_empty(), "retried events must conflict");

    let mut same_day = events[0].clone();
    same_day.event_time = utc_today_at(0, 0, 10);
    let mut later_same_day = same_day.clone();
    later_same_day.event_time = utc_today_at(0, 0, 20);
    let mut next_day = same_day.clone();
    next_day.event_id = format!("{}-reemitted", unique_event_id());
    next_day.event_time = utc_today_at(0, 0, 10) - chrono::Duration::days(1);
    let mut reemitted = next_day.clone();
    reemitted.event_time = utc_today_at(0, 0, 10);
    assert_eq!(
        sink.insert_events(&[same_day]).await?.inserted_event_ids.len(),
        0,
        "an event_id already stored today dedupes regardless of event_time"
    );
    assert!(sink.insert_events(&[later_same_day]).await?.inserted_event_ids.is_empty());
    assert_eq!(sink.insert_events(&[next_day]).await?.inserted_event_ids.len(), 1);
    assert_eq!(
        sink.insert_events(&[reemitted]).await?.inserted_event_ids.len(),
        1,
        "dedupe is per UTC day, so a re-emission on another day stores a row"
    );

    let classes: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_id, tableoid::regclass::text FROM transaction_events \
         WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    let today = Utc::now().date_naive().format("%Y%m%d");
    assert_eq!(
        classes,
        vec![
            (format!("{event_prefix}-cold"), format!("transaction_events_cold_{today}")),
            (format!("{event_prefix}-hot"), format!("transaction_events_hot_{today}")),
            (format!("{event_prefix}-warm"), format!("transaction_events_warm_{today}")),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn postgres_seeded_partitions_accept_every_default_admitted_day() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let event_prefix = unique_event_id();

    // No maintenance pass has run: a pod that goes ready right after the
    // migration must still store delayed events inside the default windows.
    let mut old_hot = event(&format!("{event_prefix}-hot"));
    old_hot.event_time = Utc::now() - chrono::Duration::days(3) + chrono::Duration::minutes(1);
    let mut old_cold = event_with_type(&format!("{event_prefix}-cold"), "SIMULATION_FAILED");
    old_cold.event_time = Utc::now() - chrono::Duration::days(30) + chrono::Duration::minutes(1);
    let mut ahead = event(&format!("{event_prefix}-ahead"));
    ahead.event_time = Utc::now() + chrono::Duration::minutes(59);

    let outcome = sink.insert_events(&[old_hot, old_cold, ahead]).await?;
    assert_eq!(outcome.inserted_event_ids.len(), 3);

    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_backfills_window_and_drops_expired_days() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let now = Utc::now();

    let first = sink.maintain_partitions_at(now).await?;
    assert!(first.lock_acquired);
    assert_eq!(first.partitions_dropped, 0);
    // The migration already seeded the default windows through three days
    // ahead. Only the future-skew hour can reach one more day.
    assert!(first.partitions_created <= 3);
    let second = sink.maintain_partitions_at(now).await?;
    assert_eq!(second.partitions_created, 0, "maintenance is idempotent");
    let next_day = sink.maintain_partitions_at(now + chrono::Duration::days(1)).await?;
    assert!(next_day.partitions_created > 0, "each pass extends the look-ahead");

    let event_prefix = unique_event_id();
    sink.insert_events(&[
        event(&format!("{event_prefix}-hot")),
        event_with_type(&format!("{event_prefix}-warm"), "INGRESS_RECEIVED"),
    ])
    .await?;

    // Five days later, today's hot partition is past its 3-day window plus
    // grace; the warm partition is still inside its 7-day window.
    let later = sink.maintain_partitions_at(now + chrono::Duration::days(5)).await?;
    assert!(later.partitions_dropped > 0);
    assert_eq!(event_ids_like(&pool, &event_prefix).await?, vec![format!("{event_prefix}-warm")]);
    let today_hot = format!("transaction_events_hot_{}", now.date_naive().format("%Y%m%d"));
    assert!(!hot_partitions(&pool).await?.contains(&today_hot));
    let leftover: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(&today_hot)
        .fetch_one(&pool)
        .await?;
    assert_eq!(leftover, None, "dropped partitions are not left detached");

    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_drops_leftover_detached_partitions() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let ahead = Utc::now().date_naive() + chrono::Duration::days(3);
    let detached: bool =
        sqlx::query_scalar("SELECT transaction_events_detach_partition('hot', $1)")
            .bind(ahead)
            .fetch_one(&pool)
            .await?;
    assert!(detached);

    let outcome = sink.maintain_partitions().await?;

    assert!(outcome.partitions_dropped >= 1, "leftover detached table is dropped");
    let name = format!("transaction_events_hot_{}", ahead.format("%Y%m%d"));
    assert!(
        hot_partitions(&pool).await?.contains(&name),
        "the in-window day is recreated as an attached partition"
    );

    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_skips_ddl_that_hits_lock_timeout() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1)
        .await?
        .with_retention_config(TransactionEventRetentionConfig {
            partition_lock_timeout_ms: 200,
            ..Default::default()
        })?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&harness.database_url).await?;
    let later = Utc::now() + chrono::Duration::days(10);

    // A long-running reader of the hot class blocks DETACH's ACCESS
    // EXCLUSIVE lock but not ATTACH's SHARE UPDATE EXCLUSIVE lock.
    let mut reader = pool.begin().await?;
    sqlx::query("LOCK TABLE transaction_events_hot IN ACCESS SHARE MODE")
        .execute(&mut *reader)
        .await?;
    let before = hot_partitions(&pool).await?;

    let started = Instant::now();
    let blocked = sink.maintain_partitions_at(later).await?;
    assert!(blocked.lock_timeouts > 0, "blocked detaches are skipped, not fatal");
    assert!(started.elapsed() < Duration::from_secs(10));
    let during = hot_partitions(&pool).await?;
    assert!(before.iter().all(|name| during.contains(name)), "no hot partition was detached");

    reader.rollback().await?;
    let retried = sink.maintain_partitions_at(later).await?;
    assert_eq!(retried.lock_timeouts, 0);
    let after = hot_partitions(&pool).await?;
    assert!(before.iter().all(|name| !after.contains(name)), "the next pass drops them");

    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_does_not_leak_lock_timeout() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let ingest_pool =
        PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let retention_pool =
        PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let sink = PgTransactionEventSink::new_with_retention_pool(ingest_pool, retention_pool.clone());
    sink.maintain_partitions().await?;

    let lock_timeout: String =
        sqlx::query_scalar("SHOW lock_timeout").fetch_one(&retention_pool).await?;
    assert_eq!(
        lock_timeout, "0",
        "SET LOCAL lock_timeout leaked onto the pooled connection: {lock_timeout}"
    );

    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_skips_when_another_replica_holds_lock() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let mut transaction = pool.begin().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(744697762131337711)")
        .fetch_one(&mut *transaction)
        .await?;
    assert!(locked);

    let outcome = sink.maintain_partitions().await?;
    assert!(!outcome.lock_acquired);
    assert_eq!(outcome.partitions_created, 0);
    assert_eq!(outcome.partitions_dropped, 0);

    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn postgres_maintenance_uses_retention_pool_when_ingest_is_busy() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let ingest_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(500))
        .connect(&harness.database_url)
        .await?;
    let retention_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(1))
        .connect(&harness.database_url)
        .await?;
    let sink = PgTransactionEventSink::new_with_retention_pool(ingest_pool.clone(), retention_pool);

    let _held = ingest_pool.acquire().await?;
    let outcome =
        tokio::time::timeout(Duration::from_secs(10), sink.maintain_partitions())
            .await
            .expect("maintenance should use the retention pool instead of waiting on ingest")?;
    assert!(outcome.lock_acquired);

    Ok(())
}

/// Mirrors production roles: the migration role owns the schema and tables,
/// and the runtime role only has DML plus EXECUTE on the partition functions.
#[tokio::test]
async fn postgres_runtime_role_maintains_partitions_through_definer_functions() -> anyhow::Result<()>
{
    let harness = PostgresHarness::new().await?;
    let admin = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    for statement in [
        "CREATE ROLE audit_archiver_migration LOGIN PASSWORD 'migration'",
        "CREATE ROLE audit_archiver LOGIN PASSWORD 'runtime'",
        "CREATE ROLE unrelated LOGIN PASSWORD 'unrelated'",
        "ALTER SCHEMA public OWNER TO audit_archiver_migration",
    ] {
        admin.execute(statement).await?;
    }

    PgTransactionEventSink::migrate(&harness.url_for("audit_archiver_migration", "migration"))
        .await?;

    let runtime_url = harness.url_for("audit_archiver", "runtime");
    let sink = PgTransactionEventSink::connect(&runtime_url, 1).await?;
    sink.check_schema_ready().await?;
    assert!(sink.maintain_partitions().await?.lock_acquired);
    let tomorrow = sink.maintain_partitions_at(Utc::now() + chrono::Duration::days(1)).await?;
    assert!(tomorrow.partitions_created > 0, "runtime role can create partitions");

    let event_id = unique_event_id();
    sink.insert_events(&[event(&event_id)]).await?;
    assert_eq!(sink.events_by_block_number(123, 10).await?.len(), 1);

    let later = sink.maintain_partitions_at(Utc::now() + chrono::Duration::days(5)).await?;
    assert!(later.partitions_dropped > 0, "runtime role can drop expired partitions");

    let runtime = PgPoolOptions::new().max_connections(1).connect(&runtime_url).await?;
    let partition = format!(
        "transaction_events_hot_{}",
        (Utc::now().date_naive() + chrono::Duration::days(3)).format("%Y%m%d")
    );
    let direct_drop = runtime.execute(format!("DROP TABLE {partition}").as_str()).await;
    assert!(direct_drop.is_err(), "runtime role must not own partitions");

    let unrelated = PgPoolOptions::new()
        .max_connections(1)
        .connect(&harness.url_for("unrelated", "unrelated"))
        .await?;
    let call = sqlx::query("SELECT transaction_events_detach_partition('hot', current_date)")
        .execute(&unrelated)
        .await;
    assert!(call.is_err(), "partition functions are not executable by PUBLIC");

    Ok(())
}

#[tokio::test]
async fn postgres_insert_fails_fast_when_conflicting_row_is_locked() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&harness.database_url).await?;
    let sink = PgTransactionEventSink::new(pool.clone());
    let event_id = unique_event_id();
    let pending = event(&event_id);

    let mut held = pool.begin().await?;
    sqlx::query(
        "INSERT INTO transaction_events \
         (event_id, schema_version, event_time, event_date, retention_class, producer, \
          event_type, network, data) \
         VALUES ($1, 'transaction-event/v1', $2, $3, 'hot', 'base-builder', \
                 'BUILDER_ACCEPTED', 'base-mainnet', '{}'::jsonb)",
    )
    .bind(&event_id)
    .bind(pending.event_time)
    .bind(pending.event_time.date_naive())
    .execute(&mut *held)
    .await?;

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        sink.insert_events(std::slice::from_ref(&pending)),
    )
    .await
    .expect("insert should fail lock waits instead of blocking until the test times out");
    assert!(result.is_err(), "conflicting insert should fail after lock_timeout retries");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "three lock_timeout attempts should take more than one 1s wait"
    );

    held.rollback().await?;
    sink.insert_events(&[pending]).await?;
    cleanup(&pool, &event_id).await;
    Ok(())
}

#[tokio::test]
async fn postgres_insert_does_not_leak_lock_timeout() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let sink = PgTransactionEventSink::new(pool.clone());
    let event_id = unique_event_id();
    sink.insert_events(&[event(&event_id)]).await?;

    let lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&pool).await?;
    assert_ne!(
        lock_timeout, "1s",
        "SET LOCAL lock_timeout leaked onto the pooled connection: {lock_timeout}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Postgres (set DATABASE_URL)"]
async fn postgres_sink_persists_and_dedupes_by_event_id() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let event_id = unique_event_id();
    PgTransactionEventSink::migrate(&database_url).await.unwrap();
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.unwrap();
    cleanup(&pool, &event_id).await;

    let sink = PgTransactionEventSink::connect(&database_url, 2).await.unwrap();
    let event = event(&event_id);

    let first = sink.insert_events(std::slice::from_ref(&event)).await.unwrap();
    assert!(first.inserted_event_ids.contains(&event_id));

    let second = sink.insert_events(std::slice::from_ref(&event)).await.unwrap();
    assert!(second.inserted_event_ids.is_empty());

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM transaction_events WHERE event_id = $1")
            .bind(&event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 1);

    cleanup(&pool, &event_id).await;
}

#[tokio::test]
async fn postgres_query_finds_events_by_normalized_tx_hash() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let event_id = unique_event_id();
    sink.insert_events(&[event(&event_id)]).await?;

    let lowercase = sink
        .events_by_transaction_hash(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            10,
        )
        .await?;
    assert_eq!(lowercase.len(), 1);
    assert_eq!(lowercase[0].event.event_id, event_id);

    let mixed_case_hash =
        "0x1111111111111111111111111111111111111111111111111111111111111111".to_ascii_uppercase();
    let mixed_case = sink.events_by_transaction_hash(&mixed_case_hash, 10).await?;
    assert_eq!(mixed_case.len(), 1);
    assert_eq!(mixed_case[0].event.event_id, event_id);

    let block_events = sink.events_by_block_number(123, 10).await?;
    assert_eq!(block_events.len(), 1);
    assert_eq!(block_events[0].event.event_id, event_id);

    Ok(())
}

#[tokio::test]
async fn postgres_query_finds_legacy_uppercase_tx_hash_rows() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_id = unique_event_id();
    sink.insert_events(&[event(&event_id)]).await?;
    sqlx::query("UPDATE transaction_events SET tx_hash = $1 WHERE event_id = $2")
        .bind("0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .bind(&event_id)
        .execute(&pool)
        .await?;

    let records = sink
        .events_by_transaction_hash(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            10,
        )
        .await?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].event.event_id, event_id);

    Ok(())
}

#[tokio::test]
async fn postgres_rejected_query_returns_bounded_newest_first() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let event_prefix = unique_event_id();

    let mut older = event_with_type(&format!("{event_prefix}-old"), "SIMULATION_FAILED");
    older.event_time = Utc::now() - chrono::Duration::seconds(30);
    let mut newer = event_with_type(&format!("{event_prefix}-new"), "BUILDER_REJECTED");
    newer.event_time = Utc::now();
    let accepted = event_with_type(&format!("{event_prefix}-ok"), "BUILDER_ACCEPTED");
    sink.insert_events(&[older, newer, accepted]).await?;

    let records = sink
        .rejected_transaction_events(RejectedTransactionEventQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].event.event_id, format!("{event_prefix}-new"));
    assert_eq!(records[0].event.event_type.to_string(), "BUILDER_REJECTED");

    Ok(())
}
