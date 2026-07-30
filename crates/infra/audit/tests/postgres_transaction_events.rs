//! Postgres integration tests for transaction event ingest.
//!
//! Run with:
//!
//! ```bash
//! DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
//!   cargo test -p audit-archiver-lib --test postgres_transaction_events -- --ignored
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use audit_archiver_lib::{
    MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE, PgTransactionEventSink,
    TransactionEventRetentionClass, TransactionEventRetentionConfig,
    TransactionEventSchemaReadinessError, TransactionEventSink,
};
use base_observability_events::{TransactionEvent, TransactionEventType};
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
    serde_json::from_value(json!({
        "schema_version": "transaction-event/v1",
        "event_id": event_id,
        "event_time": Utc::now(),
        "producer": "base-builder",
        "event_type": "BUILDER_ACCEPTED",
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
    for table_name in
        ["transaction_events_hot", "transaction_events_warm", "transaction_events_cold"]
    {
        let statement = format!("DELETE FROM {table_name} WHERE event_id = $1");
        let _ = pool.execute(sqlx::query(&statement).bind(event_id)).await;
    }
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
    assert!(err.to_string().contains("002_transaction_event_retention.sql"));

    Ok(())
}

#[test]
fn transaction_events_migration_version_matches_sqlx_migration_metadata() -> anyhow::Result<()> {
    assert_eq!(
        PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?,
        2,
        "002_transaction_event_retention.sql should resolve to sqlx migration version 2"
    );
    Ok(())
}

#[tokio::test]
async fn retention_migration_preserves_legacy_transaction_events() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let event_id = unique_event_id();

    sqlx::raw_sql(include_str!("../migrations/001_transaction_events.sql")).execute(&pool).await?;
    sqlx::query(
        "INSERT INTO transaction_events \
         (event_id, schema_version, event_time, producer, event_type, data) \
         VALUES ($1, 'transaction-event/v1', now(), 'base-builder', 'BUILDER_ACCEPTED', '{}')",
    )
    .bind(&event_id)
    .execute(&pool)
    .await?;

    sqlx::raw_sql(include_str!("../migrations/002_transaction_event_retention.sql"))
        .execute(&pool)
        .await?;

    let count: (i64,) =
        sqlx::query_as("SELECT count(*) FROM transaction_events WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(count.0, 1);

    Ok(())
}

#[tokio::test]
async fn transaction_events_ready_after_required_migration() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;

    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;

    sink.check_schema_ready().await?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let schema: (String, i64, bool) = sqlx::query_as(
        "SELECT \
         (SELECT relkind::TEXT FROM pg_class \
          WHERE oid = 'transaction_events'::regclass), \
         (SELECT count(*) FROM pg_class \
          WHERE oid IN ( \
            'transaction_events_hot'::regclass, \
            'transaction_events_warm'::regclass, \
            'transaction_events_cold'::regclass \
          ) AND relkind = 'p'), \
         to_regprocedure( \
            'maintain_transaction_event_partitions(timestamptz,integer,integer,integer,integer)' \
         ) IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(schema, ("r".to_string(), 3, true));

    let applied: (bool,) =
        sqlx::query_as("SELECT success FROM _sqlx_migrations WHERE version = $1")
            .bind(PgTransactionEventSink::required_migration_version().map_err(anyhow::Error::msg)?)
            .fetch_one(&pool)
            .await?;
    assert!(applied.0);

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
        sqlx::query_as("SELECT COUNT(*) FROM transaction_events_hot WHERE event_id LIKE $1")
            .bind(format!("{event_prefix}-%"))
            .fetch_one(&pool)
            .await?;
    assert_eq!(count.0, i64::try_from(event_count)?);
    let placement: (String, String) = sqlx::query_as(
        "SELECT retention_class, tableoid::regclass::TEXT \
         FROM transaction_events_hot \
         WHERE event_id = $1",
    )
    .bind(format!("{event_prefix}-0"))
    .fetch_one(&pool)
    .await?;
    assert_eq!(placement.0, TransactionEventRetentionClass::Hot.to_string());
    assert!(placement.1.starts_with("transaction_events_hot_"));

    Ok(())
}

#[tokio::test]
async fn postgres_sink_routes_events_to_class_specific_tables() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let event_prefix = unique_event_id();

    let mut hot = event(&format!("{event_prefix}-hot"));
    hot.event_type = TransactionEventType::BuilderAccepted;
    let mut warm = event(&format!("{event_prefix}-warm"));
    warm.event_type = TransactionEventType::IngressReceived;
    let mut cold = event(&format!("{event_prefix}-cold"));
    cold.event_type = TransactionEventType::SimulationFailed;

    let outcome = sink.insert_events(&[hot, warm, cold]).await?;
    assert_eq!(outcome.inserted_event_ids.len(), 3);

    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    for (class, table_name) in [
        ("hot", "transaction_events_hot"),
        ("warm", "transaction_events_warm"),
        ("cold", "transaction_events_cold"),
    ] {
        let statement = format!(
            "SELECT count(*) FROM {table_name} WHERE event_id = $1 AND retention_class = $2"
        );
        let count: (i64,) = sqlx::query_as(&statement)
            .bind(format!("{event_prefix}-{class}"))
            .bind(class)
            .fetch_one(&pool)
            .await?;
        assert_eq!(count.0, 1, "{class} event was not routed to {table_name}");
    }

    let combined_count: (i64,) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM transaction_events_hot WHERE event_id LIKE $1) + \
            (SELECT count(*) FROM transaction_events_warm WHERE event_id LIKE $1) + \
            (SELECT count(*) FROM transaction_events_cold WHERE event_id LIKE $1)",
    )
    .bind(format!("{event_prefix}-%"))
    .fetch_one(&pool)
    .await?;
    assert_eq!(combined_count.0, 3);

    Ok(())
}

#[tokio::test]
async fn postgres_partition_maintenance_creates_and_drops_class_windows() -> anyhow::Result<()> {
    let harness = PostgresHarness::new().await?;
    PgTransactionEventSink::migrate(&harness.database_url).await?;
    let sink = PgTransactionEventSink::connect(&harness.database_url, 1).await?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&harness.database_url).await?;
    let expired_day = Utc::now().date_naive() - chrono::Duration::days(10);
    for class in ["hot", "warm", "cold"] {
        sqlx::query("SELECT create_transaction_event_partition($1, $2)")
            .bind(class)
            .bind(expired_day)
            .execute(&pool)
            .await?;
    }

    let outcomes = sink
        .maintain_partitions(TransactionEventRetentionConfig {
            premake_days: 2,
            hot_days: 1,
            warm_days: 2,
            cold_days: 3,
        })
        .await?;
    assert_eq!(outcomes.len(), 3);

    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT replace(parent.relname, 'transaction_events_', ''), count(*) \
         FROM pg_inherits AS inheritance \
         JOIN pg_class AS parent ON parent.oid = inheritance.inhparent \
         WHERE parent.relname IN ( \
            'transaction_events_hot', \
            'transaction_events_warm', \
            'transaction_events_cold' \
         ) \
         GROUP BY parent.relname \
         ORDER BY parent.relname",
    )
    .fetch_all(&pool)
    .await?;
    assert!(counts.iter().any(|(class, count)| class == "hot" && *count >= 3));
    assert!(counts.iter().any(|(class, count)| class == "warm" && *count >= 3));
    assert!(counts.iter().any(|(class, count)| class == "cold" && *count >= 3));
    for outcome in &outcomes {
        assert!(
            outcome.partitions_ahead_days.unwrap_or(-1) >= 2,
            "expected at least two premade future days, got {:?}",
            outcome.partitions_ahead_days
        );
    }
    for class in ["hot", "warm", "cold"] {
        let expired_partition =
            format!("transaction_events_{class}_{}", expired_day.format("%Y%m%d"));
        let exists: (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
            .bind(expired_partition)
            .fetch_one(&pool)
            .await?;
        assert!(!exists.0, "expired {class} partition was not dropped");
    }

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
        sqlx::query_as("SELECT COUNT(*) FROM transaction_events_hot WHERE event_id = $1")
            .bind(&event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 1);

    cleanup(&pool, &event_id).await;
}
