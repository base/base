//! Postgres integration tests for transaction event ingest.
//!
//! Run with:
//!
//! ```bash
//! DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
//!   cargo test -p audit-archiver-lib --test postgres_transaction_events -- --ignored
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use audit_archiver_lib::{PgTransactionEventSink, TransactionEventSink};
use base_observability_events::TransactionEvent;
use chrono::Utc;
use serde_json::json;
use sqlx::{Executor, PgPool, postgres::PgPoolOptions};

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
    let _ = pool
        .execute(sqlx::query("DELETE FROM transaction_events WHERE event_id = $1").bind(event_id))
        .await;
}

#[tokio::test]
#[ignore = "requires a running Postgres (set DATABASE_URL)"]
async fn postgres_sink_persists_and_dedupes_by_event_id() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let event_id = unique_event_id();
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
