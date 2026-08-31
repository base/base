//! Postgres integration tests for transaction event ingest.
//!
//! Run with:
//!
//! ```bash
//! DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
//!   cargo test -p audit-archiver-lib --test postgres_transaction_events -- --ignored
//! ```

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use audit_archiver_lib::{
    MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE, PgTransactionEventSink, RejectedTransactionEventQuery,
    TransactionEventRetentionConfig, TransactionEventSchemaReadinessError, TransactionEventSink,
};
use base_observability_events::TransactionEvent;
use chrono::Utc;
use serde_json::json;
use sqlx::{Executor, PgPool, postgres::PgPoolOptions};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

struct PostgresHarness {
    database_url: String,
    _container: testcontainers::ContainerAsync<Postgres>,
}

impl PostgresHarness {
    async fn new() -> anyhow::Result<Self> {
        let container = Postgres::default().start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        Ok(Self { database_url, _container: container })
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

async fn cleanup(pool: &PgPool, event_id: &str) {
    let _ = pool
        .execute(sqlx::query("DELETE FROM transaction_events WHERE event_id = $1").bind(event_id))
        .await;
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
    assert!(err.to_string().contains("001_transaction_events.sql"));

    Ok(())
}

#[test]
fn transaction_events_migration_version_matches_sqlx_migration_metadata() -> anyhow::Result<()> {
    assert_eq!(
        PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?,
        1,
        "001_transaction_events.sql should resolve to sqlx migration version 1"
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
async fn postgres_retention_index_is_applied() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
            SELECT 1 FROM pg_indexes \
            WHERE schemaname = 'public' \
              AND indexname = 'transaction_events_event_type_ingested_at_idx' \
        )",
    )
    .fetch_one(&pool)
    .await?;
    assert!(exists);

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
async fn postgres_retention_deletes_expired_rows_in_batches() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events = (0..3).map(|index| event(&format!("{event_prefix}-{index}"))).collect::<Vec<_>>();
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '10 days' \
         WHERE event_id IN ($1, $2)",
    )
    .bind(format!("{event_prefix}-0"))
    .bind(format!("{event_prefix}-1"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 1,
            max_batches: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.rows_deleted, 2);
    assert_eq!(outcome.hot_rows_deleted, 2);
    assert_eq!(outcome.batches, 5);

    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM transaction_events WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(remaining, vec![format!("{event_prefix}-2")]);

    Ok(())
}

#[tokio::test]
async fn postgres_retention_uses_per_class_cutoffs() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events = [
        event_with_type(&format!("{event_prefix}-hot-old"), "BUILDER_ACCEPTED"),
        event_with_type(&format!("{event_prefix}-warm-mid"), "INGRESS_RECEIVED"),
        event_with_type(&format!("{event_prefix}-cold-mid"), "SIMULATION_FAILED"),
        event_with_type(&format!("{event_prefix}-hot-fresh"), "BUILDER_ACCEPTED"),
    ];
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '4 days' \
         WHERE event_id = $1",
    )
    .bind(format!("{event_prefix}-hot-old"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '8 days' \
         WHERE event_id IN ($1, $2)",
    )
    .bind(format!("{event_prefix}-warm-mid"))
    .bind(format!("{event_prefix}-cold-mid"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 3,
            warm_days: 7,
            cold_days: 30,
            delete_batch_size: 10,
            max_batches: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 1);
    assert_eq!(outcome.warm_rows_deleted, 1);
    assert_eq!(outcome.cold_rows_deleted, 0);
    assert_eq!(outcome.rows_deleted, 2);

    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM transaction_events WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        remaining,
        vec![format!("{event_prefix}-cold-mid"), format!("{event_prefix}-hot-fresh")]
    );

    Ok(())
}

#[tokio::test]
async fn postgres_expires_at_lifecycle_events_with_peer_classes() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events = [
        event_with_type(&format!("{event_prefix}-admission"), "TXPOOL_SEND_RAW_TRANSACTION"),
        event_with_type(
            &format!("{event_prefix}-validity-admission"),
            "TXPOOL_SEND_RAW_TRANSACTION_VALIDITY",
        ),
        event_with_type(&format!("{event_prefix}-rejected"), "BUILDER_REJECTED"),
        event_with_type(&format!("{event_prefix}-deferred"), "BUILDER_DEFERRED"),
        event_with_type(&format!("{event_prefix}-expired"), "BUILDER_EXPIRED"),
        event_with_type(&format!("{event_prefix}-included"), "BUILDER_INCLUDED"),
    ];
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '4 days' \
         WHERE event_id IN ($1, $2, $3)",
    )
    .bind(format!("{event_prefix}-rejected"))
    .bind(format!("{event_prefix}-deferred"))
    .bind(format!("{event_prefix}-expired"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '8 days' \
         WHERE event_id IN ($1, $2, $3)",
    )
    .bind(format!("{event_prefix}-admission"))
    .bind(format!("{event_prefix}-validity-admission"))
    .bind(format!("{event_prefix}-included"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 3,
            warm_days: 7,
            cold_days: 30,
            delete_batch_size: 10,
            max_batches: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 3);
    assert_eq!(outcome.warm_rows_deleted, 2);
    assert_eq!(outcome.cold_rows_deleted, 0);

    let remaining: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_id, event_type FROM transaction_events \
         WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(remaining, vec![(format!("{event_prefix}-included"), "BUILDER_INCLUDED".into())]);

    Ok(())
}

#[tokio::test]
async fn postgres_expire_walks_keyset_across_batches() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();
    let events: Vec<_> = (0..5).map(|i| event(&format!("{event_prefix}-hot-{i}"))).collect();
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '10 days' \
         WHERE event_id LIKE $1",
    )
    .bind(format!("{event_prefix}-%"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 2,
            max_batches: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 5);
    assert!(outcome.batches >= 3, "expected multiple keyset pages, got {}", outcome.batches);

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transaction_events WHERE event_id LIKE $1")
            .bind(format!("{event_prefix}-%"))
            .fetch_one(&pool)
            .await?;
    assert_eq!(remaining, 0);

    Ok(())
}

#[tokio::test]
async fn postgres_retention_skips_when_another_replica_holds_lock() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let mut transaction = pool.begin().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(744697762131337711)")
        .fetch_one(&mut *transaction)
        .await?;
    assert!(locked);

    let outcome = sink.expire_old_events(TransactionEventRetentionConfig::default()).await?;
    assert_eq!(outcome.rows_deleted, 0);
    assert_eq!(outcome.batches, 0);
    assert_eq!(outcome.hot_rows_deleted, 0);
    assert_eq!(outcome.warm_rows_deleted, 0);
    assert_eq!(outcome.cold_rows_deleted, 0);

    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn postgres_retention_skips_locked_expired_rows() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events =
        [event(&format!("{event_prefix}-locked")), event(&format!("{event_prefix}-unlocked"))];
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '10 days' \
         WHERE event_id IN ($1, $2)",
    )
    .bind(format!("{event_prefix}-locked"))
    .bind(format!("{event_prefix}-unlocked"))
    .execute(&pool)
    .await?;

    let mut held = pool.begin().await?;
    sqlx::query("SELECT * FROM transaction_events WHERE event_id = $1 FOR UPDATE")
        .bind(format!("{event_prefix}-locked"))
        .execute(&mut *held)
        .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 10,
            max_batches: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 1);
    assert_eq!(outcome.rows_deleted, 1);

    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM transaction_events WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(remaining, vec![format!("{event_prefix}-locked")]);

    held.rollback().await?;
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
         (event_id, schema_version, event_time, producer, event_type, network, data) \
         VALUES ($1, 'transaction-event/v1', now(), 'base-builder', 'BUILDER_ACCEPTED', \
                 'base-mainnet', '{}'::jsonb)",
    )
    .bind(&event_id)
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
async fn postgres_expire_does_not_leak_statement_timeout() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let ingest_pool =
        PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let retention_pool =
        PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let sink = PgTransactionEventSink::new_with_retention_pool(ingest_pool, retention_pool.clone());
    sink.expire_old_events(TransactionEventRetentionConfig {
        max_batches: 3,
        ..Default::default()
    })
    .await?;

    let statement_timeout: String =
        sqlx::query_scalar("SHOW statement_timeout").fetch_one(&retention_pool).await?;
    assert_eq!(
        statement_timeout, "0",
        "SET LOCAL statement_timeout leaked onto the pooled connection: {statement_timeout}"
    );

    Ok(())
}

#[tokio::test]
async fn postgres_expire_statement_timeout_advances_to_later_classes() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events = [
        event(&format!("{event_prefix}-hot")),
        event_with_type(&format!("{event_prefix}-warm"), "TXPOOL_SEND_RAW_TRANSACTION"),
        event_with_type(&format!("{event_prefix}-cold"), "SIMULATION_FAILED"),
    ];
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '10 days' \
         WHERE event_id LIKE $1",
    )
    .bind(format!("{event_prefix}-%"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 10,
            max_batches: 10,
            statement_timeout_ms: 200,
            test_hot_sleep_ms: Some(2_000),
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 0, "timed-out hot class must not fail the pass");
    assert_eq!(outcome.warm_rows_deleted, 1);
    assert_eq!(outcome.cold_rows_deleted, 1);

    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM transaction_events WHERE event_id LIKE $1 ORDER BY event_id",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(remaining, vec![format!("{event_prefix}-hot")]);

    Ok(())
}

#[tokio::test]
async fn postgres_expire_timeout_then_limit_one_deletes_hot_rows() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_prefix = unique_event_id();

    let events = [
        event(&format!("{event_prefix}-hot-a")),
        event(&format!("{event_prefix}-hot-b")),
        event_with_type(&format!("{event_prefix}-warm"), "TXPOOL_SEND_RAW_TRANSACTION"),
    ];
    sink.insert_events(&events).await?;
    sqlx::query(
        "UPDATE transaction_events SET ingested_at = now() - interval '10 days' \
         WHERE event_id LIKE $1",
    )
    .bind(format!("{event_prefix}-%"))
    .execute(&pool)
    .await?;

    let outcome = sink
        .expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 10,
            max_batches: 20,
            statement_timeout_ms: 200,
            test_hot_sleep_ms: Some(2_000),
            test_hot_sleep_min_limit: Some(2),
            ..Default::default()
        })
        .await?;
    assert_eq!(outcome.hot_rows_deleted, 2);
    assert_eq!(outcome.warm_rows_deleted, 1);

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transaction_events WHERE event_id LIKE $1")
            .bind(format!("{event_prefix}-%"))
            .fetch_one(&pool)
            .await?;
    assert_eq!(remaining, 0);

    Ok(())
}

#[tokio::test]
async fn postgres_expire_uses_retention_pool_when_ingest_is_busy() -> anyhow::Result<()> {
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
    let event_id = unique_event_id();
    sink.insert_events(&[event(&event_id)]).await?;
    sqlx::query("UPDATE transaction_events SET ingested_at = now() - interval '10 days' WHERE event_id = $1")
        .bind(&event_id)
        .execute(&ingest_pool)
        .await?;

    let _held = ingest_pool.acquire().await?;
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        sink.expire_old_events(TransactionEventRetentionConfig {
            hot_days: 7,
            warm_days: 7,
            cold_days: 7,
            delete_batch_size: 10,
            max_batches: 10,
            ..Default::default()
        }),
    )
    .await
    .expect("expire should acquire the retention pool instead of waiting on ingest")?;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "expire waited too long with the ingest pool checked out: {:?}",
        started.elapsed()
    );
    assert_eq!(outcome.hot_rows_deleted, 1);
    assert_eq!(outcome.rows_deleted, 1);

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
