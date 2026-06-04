//! HTTP ingest path for transaction observability events.

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use base_observability_events::TransactionEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder, Row, postgres::PgPoolOptions};
use tokio::net::TcpListener;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{error, info};

use crate::Metrics;

/// Default HTTP path used by Vector's HTTP output.
pub const DEFAULT_TRANSACTION_EVENT_BATCH_PATH: &str = "/v1/transaction-events/batch";

/// Default maximum number of events accepted in one HTTP request.
pub const DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE: usize = 500;

/// Default maximum serialized JSON bytes for a single event.
pub const DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES: usize = 256 * 1024;

/// Default maximum serialized JSON bytes for the event `data` field.
pub const DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES: usize = 128 * 1024;

/// Default maximum request body size for the HTTP endpoint.
pub const DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Configuration for transaction event HTTP ingest.
#[derive(Debug, Clone)]
pub struct TransactionEventIngestConfig {
    /// HTTP listen address.
    pub listen_addr: SocketAddr,
    /// HTTP path.
    pub path: String,
    /// Maximum events per request.
    pub max_batch_size: usize,
    /// Maximum serialized event size in bytes.
    pub max_event_bytes: usize,
    /// Maximum serialized `data` size in bytes.
    pub max_data_bytes: usize,
    /// Maximum request body size in bytes.
    pub max_request_bytes: usize,
}

/// Whole-request ingest status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionEventBatchStatus {
    /// Every event was newly persisted.
    Accepted,
    /// The request contained a mix of persisted, duplicate, or rejected events.
    Partial,
    /// Every valid event was a duplicate.
    Duplicate,
    /// No event was accepted because the batch only contained validation errors.
    Rejected,
}

/// Per-event ingest status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionEventItemStatus {
    /// Event was inserted into Postgres.
    Accepted,
    /// Event was already present or repeated earlier in the request.
    Duplicate,
    /// Event failed validation and was not persisted.
    Rejected,
}

/// Per-event ingest result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionEventItemResult {
    /// Event ID when it could be extracted from the JSON object.
    pub event_id: Option<String>,
    /// Event status.
    pub status: TransactionEventItemStatus,
    /// Rejection reason, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// HTTP response body for batch ingest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionEventBatchResponse {
    /// Whole-request status.
    pub status: TransactionEventBatchStatus,
    /// Number of events newly persisted.
    pub accepted: usize,
    /// Number of duplicate events.
    pub duplicate: usize,
    /// Number of rejected events.
    pub rejected: usize,
    /// Per-event results in request order.
    pub results: Vec<TransactionEventItemResult>,
}

/// Result of a database insert batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionEventInsertOutcome {
    /// Event IDs newly persisted by this insert.
    pub inserted_event_ids: HashSet<String>,
}

/// Query limits for read APIs.
pub const DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT: i64 = 500;
/// Hard maximum query result count for read APIs.
pub(crate) const MAX_TRANSACTION_EVENT_QUERY_LIMIT: i64 = 2_000;

/// Persisted transaction event row returned by audit read APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionEventRecord {
    /// Event envelope.
    #[serde(flatten)]
    pub event: TransactionEvent,
    /// Time when audit-archiver inserted the event.
    pub ingested_at: DateTime<Utc>,
}

/// Query selector for rejected transaction events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedTransactionEventQuery {
    /// Inclusive block lower bound.
    pub from_block: Option<u64>,
    /// Inclusive block upper bound.
    pub to_block: Option<u64>,
    /// Inclusive event-time lower bound.
    pub from_time: Option<DateTime<Utc>>,
    /// Exclusive event-time upper bound.
    pub to_time: Option<DateTime<Utc>>,
    /// Maximum rows to return.
    pub limit: Option<i64>,
}

/// Storage error from transaction event persistence.
#[derive(Debug, thiserror::Error)]
#[error("transaction event storage error: {source}")]
pub struct TransactionEventStorageError {
    source: anyhow::Error,
}

impl TransactionEventStorageError {
    fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

/// Durable sink for transaction observability events.
#[async_trait]
pub trait TransactionEventSink: Send + Sync {
    /// Inserts valid events and returns IDs that were newly persisted.
    async fn insert_events(
        &self,
        events: &[TransactionEvent],
    ) -> std::result::Result<TransactionEventInsertOutcome, TransactionEventStorageError>;
}

/// Postgres-backed transaction event sink.
#[derive(Debug, Clone)]
pub struct PgTransactionEventSink {
    pool: PgPool,
}

impl PgTransactionEventSink {
    /// Connects to Postgres and runs migrations.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let pool =
            PgPoolOptions::new().max_connections(max_connections).connect(database_url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Creates a sink from an existing pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Returns events for one transaction hash sorted by event time.
    pub async fn events_by_transaction_hash(
        &self,
        tx_hash: &str,
        limit: i64,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(limit);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE tx_hash = $1 \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(tx_hash)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    /// Returns events for one block number sorted by event time.
    pub async fn events_by_block_number(
        &self,
        block_number: u64,
        limit: i64,
    ) -> Result<Vec<TransactionEventRecord>> {
        let block_number = i64::try_from(block_number)?;
        let limit = normalize_limit(limit);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE block_number = $1 \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(block_number)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    /// Returns events for one block hash sorted by event time.
    pub async fn events_by_block_hash(
        &self,
        block_hash: &str,
        limit: i64,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(limit);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE block_hash = $1 \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(block_hash)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    /// Returns events for one bundle UUID or bundle hash sorted by event time.
    pub async fn events_by_bundle(
        &self,
        bundle_key: &str,
        limit: i64,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(limit);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE data->>'bundle_hash' = $1 OR data->>'bundle_id' = $1 \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(bundle_key)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    /// Returns rejected transaction events sorted newest first for list views.
    pub async fn rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(query.limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT));
        let from_block = query.from_block.map(i64::try_from).transpose()?;
        let to_block = query.to_block.map(i64::try_from).transpose()?;

        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE event_type IN ('SIMULATION_REJECTED', 'BUILDER_REJECTED') \
               AND ($1::BIGINT IS NULL OR block_number >= $1) \
               AND ($2::BIGINT IS NULL OR block_number <= $2) \
               AND ($3::TIMESTAMPTZ IS NULL OR event_time >= $3) \
               AND ($4::TIMESTAMPTZ IS NULL OR event_time < $4) \
             ORDER BY event_time DESC, ingested_at DESC, event_id DESC \
             LIMIT $5",
        )
        .bind(from_block)
        .bind(to_block)
        .bind(query.from_time)
        .bind(query.to_time)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }
}

fn normalize_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_TRANSACTION_EVENT_QUERY_LIMIT)
}

fn record_from_row(row: sqlx::postgres::PgRow) -> Result<TransactionEventRecord> {
    let block_number: Option<i64> = row.try_get("block_number")?;
    let block_number = block_number.map(u64::try_from).transpose()?;
    let data: Value = row.try_get("data")?;

    let raw_event = serde_json::json!({
        "schema_version": row.try_get::<String, _>("schema_version")?,
        "event_id": row.try_get::<String, _>("event_id")?,
        "event_time": row.try_get::<DateTime<Utc>, _>("event_time")?,
        "producer": row.try_get::<String, _>("producer")?,
        "event_type": row.try_get::<String, _>("event_type")?,
        "network": row.try_get::<Option<String>, _>("network")?,
        "tx_hash": row.try_get::<Option<String>, _>("tx_hash")?,
        "block_hash": row.try_get::<Option<String>, _>("block_hash")?,
        "block_number": block_number,
        "payload_id": row.try_get::<Option<String>, _>("payload_id")?,
        "request_id": row.try_get::<Option<String>, _>("request_id")?,
        "data": data,
    });
    let event = serde_json::from_value(raw_event)?;
    Ok(TransactionEventRecord { event, ingested_at: row.try_get("ingested_at")? })
}

#[async_trait]
impl TransactionEventSink for PgTransactionEventSink {
    async fn insert_events(
        &self,
        events: &[TransactionEvent],
    ) -> std::result::Result<TransactionEventInsertOutcome, TransactionEventStorageError> {
        if events.is_empty() {
            return Ok(TransactionEventInsertOutcome { inserted_event_ids: HashSet::new() });
        }

        let mut query_builder = QueryBuilder::new(
            "INSERT INTO transaction_events \
             (event_id, schema_version, event_time, producer, event_type, network, tx_hash, \
              block_hash, block_number, payload_id, request_id, data) ",
        );

        query_builder.push_values(events, |mut row, event| {
            let tx_hash = event.tx_hash.map(|hash| hash.to_string());
            let block_hash = event.block_hash.map(|hash| hash.to_string());
            let block_number = event.block_number.and_then(|number| i64::try_from(number).ok());
            let producer = event.producer.to_string();
            let event_type = event.event_type.to_string();
            let data = Value::Object(event.data.clone());

            row.push_bind(&event.event_id)
                .push_bind(&event.schema_version)
                .push_bind(event.event_time)
                .push_bind(producer)
                .push_bind(event_type)
                .push_bind(&event.network)
                .push_bind(tx_hash)
                .push_bind(block_hash)
                .push_bind(block_number)
                .push_bind(&event.payload_id)
                .push_bind(&event.request_id)
                .push_bind(data);
        });

        query_builder.push(" ON CONFLICT (event_id) DO NOTHING RETURNING event_id");

        let rows: Vec<(String,)> = query_builder
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(|source| TransactionEventStorageError::new(source.into()))?;

        Ok(TransactionEventInsertOutcome {
            inserted_event_ids: rows.into_iter().map(|(event_id,)| event_id).collect(),
        })
    }
}

#[derive(Clone)]
struct TransactionEventIngestState {
    sink: Arc<dyn TransactionEventSink>,
    config: TransactionEventIngestConfig,
}

/// Starts the transaction event HTTP ingest server.
pub async fn serve_transaction_event_ingest(
    sink: Arc<dyn TransactionEventSink>,
    config: TransactionEventIngestConfig,
) -> Result<()> {
    let listen_addr = config.listen_addr;
    let path = config.path.clone();
    let app = transaction_event_router(sink, config);
    let listener = TcpListener::bind(listen_addr).await?;
    info!(%listen_addr, %path, "transaction event HTTP ingest server started");
    axum::serve(listener, app).await?;
    Ok(())
}

fn transaction_event_router(
    sink: Arc<dyn TransactionEventSink>,
    config: TransactionEventIngestConfig,
) -> Router {
    let max_request_bytes = config.max_request_bytes;
    let path = config.path.clone();
    let state = Arc::new(TransactionEventIngestState { sink, config });

    Router::new()
        .route(&path, post(transaction_event_batch_handler))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_request_bytes))
        .with_state(state)
}

async fn transaction_event_batch_handler(
    State(state): State<Arc<TransactionEventIngestState>>,
    body: Bytes,
) -> Response {
    ingest_transaction_event_batch(&state, body).await.into_response()
}

async fn ingest_transaction_event_batch(
    state: &TransactionEventIngestState,
    body: Bytes,
) -> (StatusCode, Json<TransactionEventBatchResponse>) {
    let events = match parse_transaction_event_ndjson(&body) {
        Ok(events) => events,
        Err(reason) => {
            Metrics::transaction_events_rejected().increment(1);
            return (
                StatusCode::BAD_REQUEST,
                Json(TransactionEventBatchResponse {
                    status: TransactionEventBatchStatus::Rejected,
                    accepted: 0,
                    duplicate: 0,
                    rejected: 1,
                    results: vec![TransactionEventItemResult {
                        event_id: None,
                        status: TransactionEventItemStatus::Rejected,
                        reason: Some(reason),
                    }],
                }),
            );
        }
    };

    Metrics::transaction_event_batch_size().record(events.len() as f64);
    Metrics::transaction_events_received().increment(events.len() as u64);

    let mut results = Vec::with_capacity(events.len());
    if events.is_empty() {
        Metrics::transaction_events_rejected().increment(1);
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(TransactionEventBatchResponse {
                status: TransactionEventBatchStatus::Rejected,
                accepted: 0,
                duplicate: 0,
                rejected: 1,
                results: vec![TransactionEventItemResult {
                    event_id: None,
                    status: TransactionEventItemStatus::Rejected,
                    reason: Some("batch must contain at least one event".to_string()),
                }],
            }),
        );
    }

    if events.len() > state.config.max_batch_size {
        Metrics::transaction_events_rejected().increment(events.len() as u64);
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(TransactionEventBatchResponse {
                status: TransactionEventBatchStatus::Rejected,
                accepted: 0,
                duplicate: 0,
                rejected: events.len(),
                results: vec![TransactionEventItemResult {
                    event_id: None,
                    status: TransactionEventItemStatus::Rejected,
                    reason: Some(format!(
                        "batch size {} exceeds maximum {}",
                        events.len(),
                        state.config.max_batch_size
                    )),
                }],
            }),
        );
    }

    let mut seen = HashSet::new();
    let mut valid_events = Vec::new();
    for raw_event in events {
        match validate_transaction_event(raw_event, &state.config) {
            Ok(event) => {
                if seen.insert(event.event_id.clone()) {
                    results.push(TransactionEventItemResult {
                        event_id: Some(event.event_id.clone()),
                        status: TransactionEventItemStatus::Accepted,
                        reason: None,
                    });
                    valid_events.push(event);
                } else {
                    Metrics::transaction_events_duplicate().increment(1);
                    results.push(TransactionEventItemResult {
                        event_id: Some(event.event_id),
                        status: TransactionEventItemStatus::Duplicate,
                        reason: Some("duplicate event_id within request".to_string()),
                    });
                }
            }
            Err(rejection) => {
                Metrics::transaction_events_validation_failures().increment(1);
                Metrics::transaction_events_rejected().increment(1);
                results.push(TransactionEventItemResult {
                    event_id: rejection.event_id,
                    status: TransactionEventItemStatus::Rejected,
                    reason: Some(rejection.reason),
                });
            }
        }
    }

    if valid_events.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(response_from_results(results, &HashSet::new())),
        );
    }

    let write_start = Instant::now();
    let insert_outcome = match state.sink.insert_events(&valid_events).await {
        Ok(outcome) => outcome,
        Err(err) => {
            Metrics::transaction_event_batch_write_duration()
                .record(write_start.elapsed().as_secs_f64());
            Metrics::transaction_events_database_failures().increment(valid_events.len() as u64);
            error!(
                error = %err,
                batch_size = valid_events.len(),
                "failed to persist transaction event batch"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(TransactionEventBatchResponse {
                    status: TransactionEventBatchStatus::Rejected,
                    accepted: 0,
                    duplicate: 0,
                    rejected: 0,
                    results: vec![TransactionEventItemResult {
                        event_id: None,
                        status: TransactionEventItemStatus::Rejected,
                        reason: Some("database unavailable; retry batch".to_string()),
                    }],
                }),
            );
        }
    };
    Metrics::transaction_event_batch_write_duration().record(write_start.elapsed().as_secs_f64());

    let persisted = insert_outcome.inserted_event_ids.len();
    let db_duplicates = valid_events.len().saturating_sub(persisted);
    Metrics::transaction_events_persisted().increment(persisted as u64);
    Metrics::transaction_events_duplicate().increment(db_duplicates as u64);

    let inserted_event_ids = insert_outcome.inserted_event_ids;
    let mut accepted_by_event_id: HashMap<String, bool> =
        inserted_event_ids.iter().map(|event_id| (event_id.clone(), true)).collect();

    for result in &mut results {
        if result.status != TransactionEventItemStatus::Accepted {
            continue;
        }

        let Some(event_id) = &result.event_id else {
            continue;
        };

        if accepted_by_event_id.remove(event_id).is_none() {
            result.status = TransactionEventItemStatus::Duplicate;
            result.reason = Some("duplicate event_id".to_string());
        }
    }

    let response = response_from_results(results, &inserted_event_ids);
    (StatusCode::OK, Json(response))
}

fn parse_transaction_event_ndjson(body: &[u8]) -> std::result::Result<Vec<Value>, String> {
    let body =
        std::str::from_utf8(body).map_err(|err| format!("request body is not UTF-8: {err}"))?;

    let mut events = Vec::new();
    for (line_index, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(line).map_err(|err| {
            format!("invalid NDJSON transaction event on line {}: {err}", line_index + 1)
        })?;
        if !value.is_object() {
            return Err(format!(
                "invalid NDJSON transaction event on line {}: expected JSON object",
                line_index + 1
            ));
        }
        if value.get("events").is_some() && value.get("schema_version").is_none() {
            return Err(format!(
                "unsupported transaction event batch wrapper on line {}; send one event JSON object per line",
                line_index + 1
            ));
        }
        events.push(value);
    }
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidationRejection {
    event_id: Option<String>,
    reason: String,
}

fn validate_transaction_event(
    raw_event: Value,
    config: &TransactionEventIngestConfig,
) -> std::result::Result<TransactionEvent, ValidationRejection> {
    let event_id = raw_event.get("event_id").and_then(Value::as_str).map(ToString::to_string);

    let event_size = serde_json::to_vec(&raw_event).map_err(|err| ValidationRejection {
        event_id: event_id.clone(),
        reason: format!("event is not serializable JSON: {err}"),
    })?;
    if event_size.len() > config.max_event_bytes {
        return Err(ValidationRejection {
            event_id,
            reason: format!(
                "event size {} exceeds maximum {} bytes",
                event_size.len(),
                config.max_event_bytes
            ),
        });
    }

    let data_size = raw_event
        .get("data")
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|err| ValidationRejection {
            event_id: event_id.clone(),
            reason: format!("data is not serializable JSON: {err}"),
        })?
        .map_or(0, |data| data.len());
    if data_size > config.max_data_bytes {
        return Err(ValidationRejection {
            event_id,
            reason: format!(
                "data size {data_size} exceeds maximum {} bytes",
                config.max_data_bytes
            ),
        });
    }

    let event: TransactionEvent =
        serde_json::from_value(raw_event).map_err(|err| ValidationRejection {
            event_id: event_id.clone(),
            reason: format!("invalid transaction event envelope: {err}"),
        })?;

    event.validate().map_err(|err| ValidationRejection {
        event_id: Some(event.event_id.clone()),
        reason: err.to_string(),
    })?;

    if let Some(block_number) = event.block_number
        && i64::try_from(block_number).is_err()
    {
        return Err(ValidationRejection {
            event_id: Some(event.event_id),
            reason: "block_number exceeds Postgres BIGINT range".to_string(),
        });
    }

    Ok(event)
}

fn response_from_results(
    results: Vec<TransactionEventItemResult>,
    inserted_event_ids: &HashSet<String>,
) -> TransactionEventBatchResponse {
    let accepted = inserted_event_ids.len();
    let duplicate = results
        .iter()
        .filter(|result| result.status == TransactionEventItemStatus::Duplicate)
        .count();
    let rejected = results
        .iter()
        .filter(|result| result.status == TransactionEventItemStatus::Rejected)
        .count();

    let status = match (accepted, duplicate, rejected) {
        (0, 0, _) => TransactionEventBatchStatus::Rejected,
        (0, _, 0) => TransactionEventBatchStatus::Duplicate,
        (_, 0, 0) if accepted == results.len() => TransactionEventBatchStatus::Accepted,
        _ => TransactionEventBatchStatus::Partial,
    };

    TransactionEventBatchResponse { status, accepted, duplicate, rejected, results }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::http::StatusCode;
    use chrono::Utc;
    use serde_json::{Map, json};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeSink {
        inserted: Mutex<HashSet<String>>,
    }

    #[async_trait]
    impl TransactionEventSink for FakeSink {
        async fn insert_events(
            &self,
            events: &[TransactionEvent],
        ) -> std::result::Result<TransactionEventInsertOutcome, TransactionEventStorageError>
        {
            let mut inserted = self.inserted.lock().unwrap();
            let mut inserted_event_ids = HashSet::new();
            for event in events {
                if inserted.insert(event.event_id.clone()) {
                    inserted_event_ids.insert(event.event_id.clone());
                }
            }
            Ok(TransactionEventInsertOutcome { inserted_event_ids })
        }
    }

    fn config() -> TransactionEventIngestConfig {
        TransactionEventIngestConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            path: DEFAULT_TRANSACTION_EVENT_BATCH_PATH.to_string(),
            max_batch_size: 10,
            max_event_bytes: 4096,
            max_data_bytes: 1024,
            max_request_bytes: 16 * 1024,
        }
    }

    fn state(sink: Arc<dyn TransactionEventSink>) -> TransactionEventIngestState {
        TransactionEventIngestState { sink, config: config() }
    }

    fn event(event_id: &str) -> Value {
        json!({
            "schema_version": "transaction-event/v1",
            "event_id": event_id,
            "event_time": Utc::now(),
            "producer": "base-builder",
            "event_type": "BUILDER_ACCEPTED",
            "network": "base-mainnet",
            "tx_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "block_hash": null,
            "block_number": null,
            "payload_id": null,
            "request_id": null,
            "data": {
                "position": 1
            }
        })
    }

    fn ndjson(events: Vec<Value>) -> Bytes {
        let body = events
            .into_iter()
            .map(|event| serde_json::to_string(&event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        Bytes::from(body)
    }

    #[tokio::test]
    async fn accepts_valid_ndjson_batch() {
        let state = state(Arc::new(FakeSink::default()));
        let (status, Json(response)) = ingest_transaction_event_batch(
            &state,
            ndjson(vec![event("event-1"), event("event-2")]),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, TransactionEventBatchStatus::Accepted);
        assert_eq!(response.accepted, 2);
        assert_eq!(response.duplicate, 0);
        assert_eq!(response.rejected, 0);
    }

    #[tokio::test]
    async fn reports_duplicates_across_retries() {
        let sink = Arc::new(FakeSink::default());
        let state = state(sink);
        let request = ndjson(vec![event("event-1")]);
        let _ = ingest_transaction_event_batch(&state, request.clone()).await;

        let (status, Json(response)) = ingest_transaction_event_batch(&state, request).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, TransactionEventBatchStatus::Duplicate);
        assert_eq!(response.accepted, 0);
        assert_eq!(response.duplicate, 1);
        assert_eq!(response.rejected, 0);
    }

    #[tokio::test]
    async fn partially_accepts_batch_with_invalid_event() {
        let state = state(Arc::new(FakeSink::default()));
        let mut invalid = event("bad-event");
        invalid["tx_hash"] = json!("not-a-hash");

        let (status, Json(response)) =
            ingest_transaction_event_batch(&state, ndjson(vec![event("event-1"), invalid])).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, TransactionEventBatchStatus::Partial);
        assert_eq!(response.accepted, 1);
        assert_eq!(response.rejected, 1);
    }

    #[tokio::test]
    async fn rejects_json_batch_wrapper() {
        let state = state(Arc::new(FakeSink::default()));
        let body =
            Bytes::from(serde_json::to_string(&json!({ "events": [event("event-1")] })).unwrap());

        let (status, Json(response)) = ingest_transaction_event_batch(&state, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response.status, TransactionEventBatchStatus::Rejected);
        assert_eq!(response.rejected, 1);
        assert!(
            response.results[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("unsupported transaction event batch wrapper")
        );
    }

    #[tokio::test]
    async fn rejects_malformed_ndjson() {
        let state = state(Arc::new(FakeSink::default()));

        let (status, Json(response)) =
            ingest_transaction_event_batch(&state, Bytes::from("{not-json}\n")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response.status, TransactionEventBatchStatus::Rejected);
        assert!(response.results[0].reason.as_deref().unwrap().contains("line 1"));
    }

    #[test]
    fn rejects_unsafe_data_key() {
        let mut raw = event("event-1");
        raw["data"] = json!({ "authorization": "Bearer token" });

        let rejection = validate_transaction_event(raw, &config()).unwrap_err();

        assert!(rejection.reason.contains("forbidden key authorization"));
    }

    #[test]
    fn rejects_oversized_data() {
        let mut raw = event("event-1");
        let mut data = Map::new();
        data.insert("large".to_string(), Value::String("x".repeat(2048)));
        raw["data"] = Value::Object(data);

        let rejection = validate_transaction_event(raw, &config()).unwrap_err();

        assert!(rejection.reason.contains("data size"));
    }
}
