//! HTTP ingest path for transaction observability events.

use std::{collections::HashSet, sync::Arc, time::Instant};

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
use base_observability_events::{TransactionEvent, TransactionEventProducer, TransactionEventType};
use chrono::{DateTime, Utc};
use serde::{
    Deserialize, Serialize,
    de::{
        IntoDeserializer,
        value::{Error as SerdeValueError, StringDeserializer},
    },
};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder, Row, migrate::Migrator, postgres::PgPoolOptions};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::error;

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

/// Maximum events inserted in one Postgres statement.
///
/// Each row uses 12 bind parameters, so this stays below Postgres' 65,535 bind
/// parameter limit with room for future columns.
pub const MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE: usize = 5_000;

/// Configuration for transaction event HTTP ingest.
#[derive(Debug, Clone)]
pub struct TransactionEventIngestConfig {
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
pub const MAX_TRANSACTION_EVENT_QUERY_LIMIT: i64 = 2_000;
const REQUIRED_TRANSACTION_EVENT_MIGRATION_DESCRIPTION: &str = "transaction events";
static TRANSACTION_EVENT_MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Required sqlx migration version for transaction event storage.
fn required_transaction_event_migration_version() -> Result<i64, &'static str> {
    let mut matching_migrations = TRANSACTION_EVENT_MIGRATOR.iter().filter(|migration| {
        migration.description.as_ref() == REQUIRED_TRANSACTION_EVENT_MIGRATION_DESCRIPTION
    });
    let migration = matching_migrations.next().ok_or(
        "transaction event migration 001_transaction_events.sql must be embedded in audit migrator",
    )?;
    if matching_migrations.next().is_some() {
        return Err("transaction event migration description must be unique");
    }
    Ok(migration.version)
}

/// Persisted transaction event row returned by audit read APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Underlying storage error.
    pub source: anyhow::Error,
}

impl TransactionEventStorageError {
    /// Creates a storage error from an underlying error.
    pub const fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

/// Error returned when the Postgres transaction event schema is not ready.
#[derive(Debug, thiserror::Error)]
pub enum TransactionEventSchemaReadinessError {
    /// sqlx migration metadata is missing.
    #[error(
        "transaction-event Postgres schema is not ready: _sqlx_migrations is missing; run `audit-archiver migrate up` or the audit migration WorkflowTemplate before enabling TIPS_AUDIT_POSTGRES_URL"
    )]
    MigrationTableMissing,
    /// The transaction event migration has not completed successfully.
    #[error(
        "transaction-event Postgres schema is not ready: required sqlx migration version {required_version} for 001_transaction_events.sql has not been applied successfully; run `audit-archiver migrate up` or the audit migration WorkflowTemplate before enabling TIPS_AUDIT_POSTGRES_URL"
    )]
    RequiredMigrationMissing {
        /// Required sqlx migration version.
        required_version: i64,
    },
    /// The expected table is missing or not visible to the runtime role.
    #[error(
        "transaction-event Postgres schema is not ready: public.transaction_events is missing or not visible to the runtime role; run `audit-archiver migrate up` or the audit migration WorkflowTemplate before enabling TIPS_AUDIT_POSTGRES_URL"
    )]
    TransactionEventsRelationMissing,
    /// The expected table exists but cannot be queried by the runtime role.
    #[error(
        "transaction-event Postgres schema is not ready: runtime role cannot query public.transaction_events; verify audit_archiver privileges from 001_transaction_events.sql"
    )]
    TransactionEventsRelationUnavailable {
        /// Underlying database error.
        #[source]
        source: sqlx::Error,
    },
    /// A database query failed before readiness could be determined.
    #[error(
        "transaction-event Postgres schema readiness query failed: {source}; verify database connectivity and that the runtime role can read _sqlx_migrations"
    )]
    QueryFailed {
        /// Underlying database error.
        #[source]
        source: sqlx::Error,
    },
    /// The embedded sqlx migration metadata is internally inconsistent.
    #[error("transaction-event Postgres schema readiness metadata is invalid: {reason}")]
    MigrationMetadataInvalid {
        /// Static metadata error.
        reason: &'static str,
    },
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
    /// Required sqlx migration version for transaction event storage.
    pub fn required_migration_version() -> Result<i64, &'static str> {
        required_transaction_event_migration_version()
    }

    /// Connects to Postgres without running migrations.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let max_connections = max_connections.max(1);
        // RDS IAM auth tokens are only validated when a connection opens; open
        // connections remain usable after token expiry. sqlx does not expose a
        // pre-connect password callback, so the IAM-auth deployment path passes
        // a fresh startup token, eagerly opens the full configured pool while
        // that token is valid, and keeps those physical connections open. If
        // the pool is fully dropped after token expiry, the process should be
        // restarted so a new token is minted before rebuilding the pool.
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(max_connections)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Runs pending Postgres migrations.
    pub async fn migrate(database_url: &str) -> Result<()> {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        TRANSACTION_EVENT_MIGRATOR.run(&pool).await?;
        Ok(())
    }

    /// Creates a sink from an existing pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Checks whether transaction event Postgres storage is ready for runtime use.
    pub async fn check_schema_ready(
        &self,
    ) -> std::result::Result<(), TransactionEventSchemaReadinessError> {
        let (migration_table_exists, transaction_events_relation_exists): (bool, bool) =
            sqlx::query_as(
                "SELECT \
                    to_regclass('_sqlx_migrations') IS NOT NULL AS migration_table_exists, \
                    to_regclass('public.transaction_events') IS NOT NULL AS transaction_events_relation_exists",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|source| TransactionEventSchemaReadinessError::QueryFailed { source })?;

        if !migration_table_exists {
            return Err(TransactionEventSchemaReadinessError::MigrationTableMissing);
        }

        let required_version =
            required_transaction_event_migration_version().map_err(|source| {
                TransactionEventSchemaReadinessError::MigrationMetadataInvalid { reason: source }
            })?;
        let migration_applied: Option<bool> =
            sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = $1")
                .bind(required_version)
                .fetch_optional(&self.pool)
                .await
                .map_err(|source| TransactionEventSchemaReadinessError::QueryFailed { source })?;
        if !matches!(migration_applied, Some(true)) {
            return Err(TransactionEventSchemaReadinessError::RequiredMigrationMissing {
                required_version,
            });
        }

        if !transaction_events_relation_exists {
            return Err(TransactionEventSchemaReadinessError::TransactionEventsRelationMissing);
        }

        sqlx::query("SELECT 1 FROM transaction_events LIMIT 0").execute(&self.pool).await.map_err(
            |source| TransactionEventSchemaReadinessError::TransactionEventsRelationUnavailable {
                source,
            },
        )?;

        Ok(())
    }

    /// Checks optional transaction event storage readiness.
    pub async fn check_optional_schema_ready(
        sink: Option<&Self>,
    ) -> std::result::Result<(), TransactionEventSchemaReadinessError> {
        match sink {
            Some(sink) => sink.check_schema_ready().await,
            None => Ok(()),
        }
    }

    async fn insert_event_chunk(
        &self,
        events: &[TransactionEvent],
    ) -> std::result::Result<HashSet<String>, TransactionEventStorageError> {
        let block_numbers: Vec<Option<i64>> = events
            .iter()
            .map(|event| {
                event
                    .block_number
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|err| TransactionEventStorageError::new(err.into()))
            })
            .collect::<std::result::Result<_, _>>()?;

        let mut query_builder = QueryBuilder::new(
            "INSERT INTO transaction_events \
             (event_id, schema_version, event_time, producer, event_type, network, tx_hash, \
              block_hash, block_number, payload_id, request_id, data) ",
        );

        query_builder.push_values(
            events.iter().zip(block_numbers),
            |mut row, (event, block_number)| {
                let tx_hash = event.tx_hash.map(|hash| hash.to_string());
                let block_hash = event.block_hash.map(|hash| hash.to_string());
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
            },
        );

        query_builder.push(" ON CONFLICT (event_id) DO NOTHING RETURNING event_id");

        let rows: Vec<(String,)> = query_builder
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(|source| TransactionEventStorageError::new(source.into()))?;

        Ok(rows.into_iter().map(|(event_id,)| event_id).collect())
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
            "WITH bundle_events AS ( \
                SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
                network, tx_hash, block_hash, block_number, payload_id, request_id, data \
                FROM transaction_events \
                WHERE data ? 'bundle_hash' AND data->>'bundle_hash' = $1 \
                UNION ALL \
                SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
                network, tx_hash, block_hash, block_number, payload_id, request_id, data \
                FROM transaction_events \
                WHERE data ? 'bundle_id' AND data->>'bundle_id' = $1 \
             ), deduped AS ( \
                SELECT DISTINCT ON (event_id) * FROM bundle_events \
                ORDER BY event_id, event_time ASC, ingested_at ASC \
             ) \
             SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM deduped \
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
             WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED') \
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

    let event = TransactionEvent {
        schema_version: row.try_get("schema_version")?,
        event_id: row.try_get("event_id")?,
        event_time: row.try_get("event_time")?,
        producer: parse_transaction_event_producer(row.try_get("producer")?)?,
        event_type: parse_transaction_event_type(row.try_get("event_type")?)?,
        network: row.try_get("network")?,
        tx_hash: row
            .try_get::<Option<String>, _>("tx_hash")?
            .map(|hash| hash.parse())
            .transpose()?,
        block_hash: row
            .try_get::<Option<String>, _>("block_hash")?
            .map(|hash| hash.parse())
            .transpose()?,
        block_number,
        payload_id: row.try_get("payload_id")?,
        request_id: row.try_get("request_id")?,
        data: data
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("transaction event data column is not a JSON object"))?,
    };
    Ok(TransactionEventRecord { event, ingested_at: row.try_get("ingested_at")? })
}

fn parse_transaction_event_producer(producer: String) -> Result<TransactionEventProducer> {
    let deserializer: StringDeserializer<SerdeValueError> = producer.into_deserializer();
    Ok(TransactionEventProducer::deserialize(deserializer)?)
}

fn parse_transaction_event_type(event_type: String) -> Result<TransactionEventType> {
    let deserializer: StringDeserializer<SerdeValueError> = event_type.into_deserializer();
    Ok(TransactionEventType::deserialize(deserializer)?)
}

fn metric_len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn metric_len_f64(len: usize) -> f64 {
    f64::from(u32::try_from(len).unwrap_or(u32::MAX))
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

        let mut inserted_event_ids = HashSet::new();
        for chunk in events.chunks(MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE) {
            inserted_event_ids.extend(self.insert_event_chunk(chunk).await?);
        }

        Ok(TransactionEventInsertOutcome { inserted_event_ids })
    }
}

#[derive(Clone)]
struct TransactionEventIngestState {
    sink: Arc<dyn TransactionEventSink>,
    config: TransactionEventIngestConfig,
}

impl TransactionEventIngestConfig {
    /// Builds the Vector-facing transaction event ingest router.
    ///
    /// The route-specific request body limit is applied only to this router, so
    /// it can be mounted alongside other HTTP services without changing their
    /// limits.
    pub fn into_router(mut self, sink: Arc<dyn TransactionEventSink>) -> Router {
        self.max_batch_size = self.max_batch_size.min(MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE);
        let max_request_bytes = self.max_request_bytes;
        let path = self.path.clone();
        let state = Arc::new(TransactionEventIngestState { sink, config: self });

        Router::new()
            .route(&path, post(transaction_event_batch_handler))
            .layer(DefaultBodyLimit::disable())
            .layer(RequestBodyLimitLayer::new(max_request_bytes))
            .with_state(state)
    }
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
    let events = match parse_transaction_event_ndjson(&body, state.config.max_batch_size) {
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

    Metrics::transaction_event_batch_size().record(metric_len_f64(events.len()));
    Metrics::transaction_events_received().increment(metric_len_u64(events.len()));

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
            Metrics::transaction_events_database_failures()
                .increment(metric_len_u64(valid_events.len()));
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
    Metrics::transaction_events_persisted().increment(metric_len_u64(persisted));
    Metrics::transaction_events_duplicate().increment(metric_len_u64(db_duplicates));

    let inserted_event_ids = insert_outcome.inserted_event_ids;
    let mut accepted_ids = inserted_event_ids.clone();

    for result in &mut results {
        if result.status != TransactionEventItemStatus::Accepted {
            continue;
        }

        let Some(event_id) = &result.event_id else {
            continue;
        };

        if !accepted_ids.remove(event_id) {
            result.status = TransactionEventItemStatus::Duplicate;
            result.reason = Some("duplicate event_id".to_string());
        }
    }

    let response = response_from_results(results, &inserted_event_ids);
    (StatusCode::OK, Json(response))
}

#[derive(Debug, Clone)]
struct RawTransactionEvent {
    value: Value,
    byte_len: usize,
}

fn parse_transaction_event_ndjson(
    body: &[u8],
    max_batch_size: usize,
) -> std::result::Result<Vec<RawTransactionEvent>, String> {
    // The axum body limit bounds this request to max_request_bytes before this
    // point. Vector sends bounded batches, so parsing the full NDJSON request in
    // memory is acceptable; we still stop as soon as max_batch_size is exceeded.
    let body =
        std::str::from_utf8(body).map_err(|err| format!("request body is not UTF-8: {err}"))?;

    let mut events = Vec::new();
    for (line_index, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if events.len() >= max_batch_size {
            return Err(format!("batch size exceeds maximum {max_batch_size}"));
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
        events.push(RawTransactionEvent { value, byte_len: line.len() });
    }
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidationRejection {
    event_id: Option<String>,
    reason: String,
}

fn validate_transaction_event(
    raw_event: RawTransactionEvent,
    config: &TransactionEventIngestConfig,
) -> std::result::Result<TransactionEvent, ValidationRejection> {
    let event_id = raw_event.value.get("event_id").and_then(Value::as_str).map(ToString::to_string);

    if raw_event.byte_len > config.max_event_bytes {
        return Err(ValidationRejection {
            event_id,
            reason: format!(
                "event size {} exceeds maximum {} bytes",
                raw_event.byte_len, config.max_event_bytes
            ),
        });
    }

    // The parser keeps the full NDJSON line length for max_event_bytes, but
    // serde_json::Value does not retain byte spans for individual fields. The
    // data field has its own limit, so measure that subtree after parsing.
    let data_size = raw_event
        .value
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
        serde_json::from_value(raw_event.value).map_err(|err| ValidationRejection {
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

    fn raw_event(value: Value) -> RawTransactionEvent {
        let byte_len = serde_json::to_string(&value).unwrap().len();
        RawTransactionEvent { value, byte_len }
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

        let rejection = validate_transaction_event(raw_event(raw), &config()).unwrap_err();

        assert!(rejection.reason.contains("forbidden key authorization"));
    }

    #[test]
    fn rejects_oversized_data() {
        let mut raw = event("event-1");
        let mut data = Map::new();
        data.insert("large".to_string(), Value::String("x".repeat(2048)));
        raw["data"] = Value::Object(data);

        let rejection = validate_transaction_event(raw_event(raw), &config()).unwrap_err();

        assert!(rejection.reason.contains("data size"));
    }

    #[tokio::test]
    async fn rejects_too_many_ndjson_events_before_validation() {
        let mut config = config();
        config.max_batch_size = 1;
        let state = TransactionEventIngestState { sink: Arc::new(FakeSink::default()), config };

        let (status, Json(response)) = ingest_transaction_event_batch(
            &state,
            ndjson(vec![event("event-1"), event("event-2")]),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response.status, TransactionEventBatchStatus::Rejected);
        assert!(response.results[0].reason.as_deref().unwrap().contains("batch size"));
    }
}
