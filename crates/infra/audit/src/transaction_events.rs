//! HTTP ingest path for transaction observability events.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration as StdDuration, Instant},
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
use base_observability_events::{TransactionEvent, TransactionEventProducer, TransactionEventType};
use chrono::{DateTime, Duration, Utc};
use serde::{
    Deserialize, Serialize,
    de::{
        IntoDeserializer,
        value::{Error as SerdeValueError, StringDeserializer},
    },
};
use serde_json::Value;
use sqlx::{Connection, PgPool, QueryBuilder, Row, migrate::Migrator, postgres::PgPoolOptions};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{error, warn};

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

/// Session `lock_timeout` applied to each persist INSERT.
const TRANSACTION_EVENT_LOCK_TIMEOUT_SQL: &str = "SET LOCAL lock_timeout = '1s'";

/// Attempts per INSERT chunk or expire DELETE batch, including the first try.
const TRANSACTION_EVENT_DB_MAX_ATTEMPTS: u32 = 3;

/// Default days to keep high-volume proxy and builder-decision events.
pub const DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS: u32 = 3;
/// Default days to keep ingress, simulation-success, and txpool-forward events.
pub const DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS: u32 = 7;
/// Default days to keep failures, drops, inclusion, and flashblock events.
pub const DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS: u32 = 30;
/// Default number of expired rows deleted in one statement.
pub const DEFAULT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE: u32 = 10_000;
/// Default maximum delete statements per retention pass.
///
/// Shared across hot, then warm, then cold. A large hot backlog can consume
/// the whole budget and defer warmer expiry until a later pass.
pub const DEFAULT_TRANSACTION_EVENT_RETENTION_MAX_BATCHES: u32 = 1_000;
/// Default seconds between retention passes. Also the maximum wall-clock age
/// of one expire scan cycle before it is restarted at the configured LIMIT.
pub const DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS: u64 = 3_600;
/// Default Postgres statement timeout for one retention delete.
///
/// Bounds a sparse `doomed` scan so it cannot hold the advisory lock for hours.
pub const DEFAULT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS: u64 = 30_000;
const TRANSACTION_EVENT_RETENTION_ACQUIRE_TIMEOUT: StdDuration = StdDuration::from_secs(1);
const TRANSACTION_EVENT_RETENTION_LOCK_ID: i64 = 744_697_762_131_337_711;

/// Session advisory lock held on one pooled connection for a retention pass.
///
/// Unlock is always attempted. If unlock fails, or this guard is dropped
/// without unlocking, the connection is detached from the pool so Postgres
/// releases the session lock when the backend disconnects instead of leaking
/// it onto a reused pool connection.
struct RetentionAdvisoryLock {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

impl RetentionAdvisoryLock {
    async fn try_acquire(pool: &PgPool) -> Result<Option<Self>> {
        let mut conn = match pool.acquire().await {
            Ok(conn) => conn,
            Err(sqlx::Error::PoolTimedOut) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let locked = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(TRANSACTION_EVENT_RETENTION_LOCK_ID)
            .fetch_one(&mut *conn)
            .await
        {
            Ok(locked) => locked,
            Err(err) => {
                // The lock statement may have succeeded on the server even if
                // the client failed to read the result. Detach so a held
                // session lock cannot leak onto a reused pool connection.
                let _detached = conn.detach();
                return Err(err.into());
            }
        };
        if locked { Ok(Some(Self { conn: Some(conn) })) } else { Ok(None) }
    }

    fn conn(&mut self) -> &mut sqlx::PgConnection {
        self.conn.as_deref_mut().expect("retention lock connection taken")
    }

    async fn unlock(mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        if let Err(err) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(TRANSACTION_EVENT_RETENTION_LOCK_ID)
            .execute(&mut *conn)
            .await
        {
            error!(error = %err, "failed to release transaction event retention lock");
            let _detached = conn.detach();
        }
    }
}

impl Drop for RetentionAdvisoryLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _detached = conn.detach();
        }
    }
}

/// Retention class used to pick a delete cutoff for an event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEventRetentionClass {
    /// High-volume success-path and repeated builder-decision events.
    Hot,
    /// Ordinary arrival, simulation-success, and forwarding events.
    Warm,
    /// High-value inclusion, finalization, rejection, and drop events.
    Cold,
}

impl TransactionEventRetentionClass {
    /// Stable lowercase value used as a bounded metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }

    /// Classifies a transaction event at expiry.
    ///
    /// The match is exhaustive so new `TransactionEventType` variants fail to
    /// compile until they are assigned a retention class deliberately.
    pub const fn for_event_type(event_type: TransactionEventType) -> Self {
        match event_type {
            TransactionEventType::ProxyReceived
            | TransactionEventType::ProxyValidationAccepted
            | TransactionEventType::ProxyRoutedToBackend
            | TransactionEventType::ProxyBackendSuccess
            | TransactionEventType::ProxyIngressRpcAttempt
            | TransactionEventType::ProxyIngressRpcSuccess
            | TransactionEventType::BuilderConsidered
            | TransactionEventType::BuilderAccepted
            | TransactionEventType::BuilderRejected
            | TransactionEventType::BuilderDeferred
            | TransactionEventType::BuilderExpired => Self::Hot,
            TransactionEventType::IngressReceived
            | TransactionEventType::SimulationStarted
            | TransactionEventType::SimulationSucceeded
            | TransactionEventType::IngressMeteringSendAttempt
            | TransactionEventType::IngressMeteringSendSuccess
            | TransactionEventType::Pending
            | TransactionEventType::Queued
            | TransactionEventType::PendingToQueued
            | TransactionEventType::QueuedToPending
            | TransactionEventType::TxpoolBuilderForwardAttempt
            | TransactionEventType::TxpoolBuilderForwardSuccess
            | TransactionEventType::TxpoolBuilderConsumed
            | TransactionEventType::TxpoolValidatedInsertAccepted
            | TransactionEventType::TxpoolSendRawTransaction
            | TransactionEventType::TxpoolSendRawTransactionValidity => Self::Warm,
            TransactionEventType::ProxyRejected
            | TransactionEventType::ProxyValidationRejected
            | TransactionEventType::ProxyBackendFailure
            | TransactionEventType::ProxyIngressRpcFailure
            | TransactionEventType::SimulationFailed
            | TransactionEventType::IngressMeteringSendFailure
            | TransactionEventType::IngressMeteringSendDropped
            | TransactionEventType::Dropped
            | TransactionEventType::Replaced
            | TransactionEventType::Overflowed
            | TransactionEventType::TxpoolBuilderForwardFailure
            | TransactionEventType::TxpoolBuilderForwardDropped
            | TransactionEventType::TxpoolValidatedInsertRejected
            | TransactionEventType::BuilderIncluded
            | TransactionEventType::BuilderPayloadFinalized
            | TransactionEventType::BuilderFlashblockStarted
            | TransactionEventType::BuilderFlashblockPublished
            | TransactionEventType::BuilderFlashblockBuildStopped => Self::Cold,
        }
    }

    /// Event-type labels stored in Postgres for this class.
    fn event_type_labels(self) -> Vec<String> {
        self.event_types().map(|event_type| event_type.to_string()).collect()
    }

    fn event_types(self) -> impl Iterator<Item = TransactionEventType> {
        TransactionEventType::all()
            .filter(move |event_type| Self::for_event_type(*event_type) == self)
    }
}

/// Configuration for one transaction event retention pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionEventRetentionConfig {
    /// Days to keep hot event types after `ingested_at`.
    pub hot_days: u32,
    /// Days to keep warm event types after `ingested_at`.
    pub warm_days: u32,
    /// Days to keep cold event types after `ingested_at`.
    pub cold_days: u32,
    /// Maximum rows deleted in one statement.
    pub delete_batch_size: u32,
    /// Maximum delete statements in one locked pass.
    ///
    /// Shared across hot, then warm, then cold. A large hot backlog can
    /// consume the whole budget and defer warmer expiry until a later pass.
    /// Raise this if warm or cold expiry stalls behind hot deletes.
    pub max_batches: u32,
    /// Postgres `statement_timeout` applied to each expire DELETE, in
    /// milliseconds.
    pub statement_timeout_ms: u64,
    /// Seconds between retention passes. A scan cycle older than this is
    /// restarted so a bisected LIMIT cannot crawl forever. The first batch of
    /// a cycle always runs, even when this interval is shorter than one
    /// statement. A queued `LIMIT 1` after statement timeout also runs before
    /// the cycle restarts.
    pub interval_secs: u64,
    /// Test-only sleep injected into hot expire statements so a short
    /// `statement_timeout_ms` can cancel a statement without failing the pass.
    ///
    /// Leave this `None` in production. The serve binary never sets it.
    #[doc(hidden)]
    pub test_hot_sleep_ms: Option<u64>,
    /// When set with `test_hot_sleep_ms`, sleep only if the batch LIMIT is at
    /// least this value. `None` sleeps on every hot statement.
    #[doc(hidden)]
    pub test_hot_sleep_min_limit: Option<u32>,
}

impl Default for TransactionEventRetentionConfig {
    fn default() -> Self {
        Self {
            hot_days: DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS,
            warm_days: DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS,
            cold_days: DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS,
            delete_batch_size: DEFAULT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE,
            max_batches: DEFAULT_TRANSACTION_EVENT_RETENTION_MAX_BATCHES,
            statement_timeout_ms: DEFAULT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS,
            interval_secs: DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
            test_hot_sleep_ms: None,
            test_hot_sleep_min_limit: None,
        }
    }
}

impl TransactionEventRetentionConfig {
    /// Validates retention bounds before deleting expired rows.
    pub fn validate(self) -> Result<Self> {
        anyhow::ensure!(
            self.hot_days > 0 && self.hot_days <= self.warm_days,
            "transaction event hot retention days must be greater than zero and at most warm days"
        );
        anyhow::ensure!(
            self.warm_days <= self.cold_days,
            "transaction event warm retention days must be at most cold days"
        );
        anyhow::ensure!(
            (1..=100_000).contains(&self.delete_batch_size),
            "transaction event retention delete batch size must be between 1 and 100000"
        );
        anyhow::ensure!(
            (1..=1_000).contains(&self.max_batches),
            "transaction event retention max batches must be between 1 and 1000"
        );
        anyhow::ensure!(
            (1..=300_000).contains(&self.statement_timeout_ms),
            "transaction event retention statement timeout must be between 1ms and 300000ms"
        );
        anyhow::ensure!(
            (1..=604_800).contains(&self.interval_secs),
            "transaction event retention interval must be between 1 and 604800 seconds"
        );
        Ok(self)
    }

    fn statement_timeout_sql(self) -> String {
        format!("SET LOCAL statement_timeout = '{}ms'", self.statement_timeout_ms)
    }

    const fn class_days(self, class: TransactionEventRetentionClass) -> u32 {
        match class {
            TransactionEventRetentionClass::Hot => self.hot_days,
            TransactionEventRetentionClass::Warm => self.warm_days,
            TransactionEventRetentionClass::Cold => self.cold_days,
        }
    }
}

/// Result of one locked retention pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionEventRetentionOutcome {
    /// Expired rows deleted in this pass.
    pub rows_deleted: u64,
    /// Delete statements executed in this pass.
    pub batches: u64,
    /// Expired hot rows deleted in this pass.
    pub hot_rows_deleted: u64,
    /// Expired warm rows deleted in this pass.
    pub warm_rows_deleted: u64,
    /// Expired cold rows deleted in this pass.
    pub cold_rows_deleted: u64,
}

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
    retention_pool: PgPool,
}

impl PgTransactionEventSink {
    /// Required sqlx migration version for transaction event storage.
    pub fn required_migration_version() -> Result<i64, &'static str> {
        required_transaction_event_migration_version()
    }

    /// Connects to Postgres without running migrations.
    ///
    /// The ingest pool is used for persist, RPC reads, and `/readyz`.
    /// Retention uses a dedicated one-connection pool with a short acquire
    /// timeout so lock losers do not occupy ingest connections.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let max_connections = max_connections.max(1);
        let pool =
            PgPoolOptions::new().max_connections(max_connections).connect(database_url).await?;
        let retention_pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(TRANSACTION_EVENT_RETENTION_ACQUIRE_TIMEOUT)
            .connect(database_url)
            .await?;
        Ok(Self { pool, retention_pool })
    }

    /// Runs pending Postgres migrations.
    pub async fn migrate(database_url: &str) -> Result<()> {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        TRANSACTION_EVENT_MIGRATOR.run(&pool).await?;
        Ok(())
    }

    /// Creates a sink from an existing ingest pool. Retention uses the same pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool: pool.clone(), retention_pool: pool }
    }

    /// Creates a sink with a dedicated retention pool.
    pub const fn new_with_retention_pool(pool: PgPool, retention_pool: PgPool) -> Self {
        Self { pool, retention_pool }
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

    /// Deletes expired `transaction_events` rows in bounded batches.
    ///
    /// Uses a session advisory lock so only one replica expires rows at a time.
    /// Returns a zeroed outcome when another replica already holds the lock, or
    /// when the retention pool times out waiting for a connection.
    /// The lock is released after the pass, including when a batch fails. If
    /// unlock itself fails, the connection is detached so the session lock
    /// cannot leak onto a reused pool connection.
    /// Each class resumes a descending keyset on this lock holder. A statement
    /// timeout commits a `LIMIT 1` progress step when possible, then bisects
    /// the batch LIMIT. A cycle older than `interval_secs` restarts at the
    /// configured LIMIT after at least one returned attempt, but not while a
    /// `LIMIT 1` fallback is queued. A `LIMIT 1` timeout stops that class and
    /// continues the pass with the next class.
    /// Scan progress is in memory for this pass; the next pass starts new
    /// cycles.
    /// Hot rows are expired first so high-churn types free space before colder
    /// types consume the shared batch budget. That can starve warm and cold
    /// deletes in one pass; raise `max_batches` if they stall.
    pub async fn expire_old_events(
        &self,
        config: TransactionEventRetentionConfig,
    ) -> Result<TransactionEventRetentionOutcome> {
        // Public API: validate here so tests and other callers cannot skip it.
        let config = config.validate()?;
        let classes = [
            TransactionEventRetentionClass::Hot,
            TransactionEventRetentionClass::Warm,
            TransactionEventRetentionClass::Cold,
        ];

        let Some(mut lock) = RetentionAdvisoryLock::try_acquire(&self.retention_pool).await? else {
            return Ok(TransactionEventRetentionOutcome {
                rows_deleted: 0,
                batches: 0,
                hot_rows_deleted: 0,
                warm_rows_deleted: 0,
                cold_rows_deleted: 0,
            });
        };

        let outcome = async {
            let mut rows_deleted = 0u64;
            let mut batches = 0u64;
            let mut hot_rows_deleted = 0u64;
            let mut warm_rows_deleted = 0u64;
            let mut cold_rows_deleted = 0u64;
            for class in classes {
                let event_types = class.event_type_labels();
                let mut progress = RetentionClassProgress::new_cycle(Utc::now(), &config, class);
                while batches < u64::from(config.max_batches) {
                    let now = Utc::now();
                    if progress.should_end_cycle(now, config.interval_secs) {
                        warn!(
                            retention_class = class.as_str(),
                            cycle_age_secs = progress.cycle_age_secs(now),
                            batches_this_cycle = progress.batches_this_cycle,
                            "ending transaction event expire cycle after retention interval"
                        );
                        Metrics::transaction_event_retention_cycles_ended(
                            class.as_str(),
                            "interval",
                        )
                        .increment(1);
                        progress = RetentionClassProgress::new_cycle(now, &config, class);
                    }
                    let batch_limit = expire_batch_limit(
                        config.delete_batch_size,
                        progress.limit_lo,
                        progress.limit_hi,
                        progress.needs_limit_one,
                    );
                    Metrics::transaction_event_retention_effective_batch_limit(class.as_str())
                        .set(f64::from(batch_limit));
                    let outcome = delete_expired_event_batch(
                        lock.conn(),
                        &event_types,
                        &progress,
                        batch_limit,
                        class,
                        &config,
                    )
                    .await?;
                    progress.batches_this_cycle = progress.batches_this_cycle.saturating_add(1);
                    batches += 1;
                    match outcome {
                        ExpireBatchOutcome::StatementTimeout => {
                            if batch_limit <= 1 {
                                progress.needs_limit_one = true;
                                break;
                            }
                            progress.needs_limit_one = true;
                            progress.limit_hi = Some(batch_limit);
                        }
                        ExpireBatchOutcome::Deleted { selected, cursor } => {
                            if selected == 0 {
                                Metrics::transaction_event_retention_cycles_ended(
                                    class.as_str(),
                                    "exhausted",
                                )
                                .increment(1);
                                break;
                            }
                            rows_deleted += selected;
                            match class {
                                TransactionEventRetentionClass::Hot => hot_rows_deleted += selected,
                                TransactionEventRetentionClass::Warm => {
                                    warm_rows_deleted += selected
                                }
                                TransactionEventRetentionClass::Cold => {
                                    cold_rows_deleted += selected
                                }
                            }
                            Metrics::transaction_events_expired(class.as_str()).increment(selected);
                            if selected < u64::from(batch_limit) {
                                Metrics::transaction_event_retention_cycles_ended(
                                    class.as_str(),
                                    "exhausted",
                                )
                                .increment(1);
                                break;
                            }
                            progress.needs_limit_one = false;
                            progress.limit_lo = batch_limit;
                            progress.cursor = cursor;
                        }
                    }
                }
            }
            Ok(TransactionEventRetentionOutcome {
                rows_deleted,
                batches,
                hot_rows_deleted,
                warm_rows_deleted,
                cold_rows_deleted,
            })
        }
        .await;

        lock.unlock().await;
        outcome
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
        let mut ordered: Vec<&TransactionEvent> = events.iter().collect();
        ordered.sort_unstable_by(|lhs, rhs| lhs.event_id.cmp(&rhs.event_id));
        let block_numbers: Vec<Option<i64>> = ordered
            .iter()
            .map(|event| {
                event
                    .block_number
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|err| TransactionEventStorageError::new(err.into()))
            })
            .collect::<std::result::Result<_, _>>()?;
        let mut attempt = 1u32;
        loop {
            let mut query_builder = QueryBuilder::new(
                "INSERT INTO transaction_events \
                 (event_id, schema_version, event_time, producer, event_type, network, tx_hash, \
                  block_hash, block_number, payload_id, request_id, data) ",
            );
            query_builder.push_values(
                ordered.iter().copied().zip(block_numbers.iter().copied()),
                |mut row, (event, block_number)| {
                    let tx_hash = event.tx_hash.map(|hash| format!("{hash:#x}"));
                    let block_hash = event.block_hash.map(|hash| format!("{hash:#x}"));
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

            let result = async {
                let mut tx = self.pool.begin().await?;
                sqlx::query(TRANSACTION_EVENT_LOCK_TIMEOUT_SQL).execute(&mut *tx).await?;
                let rows: Vec<(String,)> =
                    query_builder.build_query_as().fetch_all(&mut *tx).await?;
                tx.commit().await?;
                Ok(rows.into_iter().map(|(event_id,)| event_id).collect())
            }
            .await;

            match result {
                Ok(ids) => return Ok(ids),
                Err(err) => {
                    let Some(reason) = persist_retry_reason(&err) else {
                        return Err(TransactionEventStorageError::new(err.into()));
                    };
                    if attempt >= TRANSACTION_EVENT_DB_MAX_ATTEMPTS {
                        return Err(TransactionEventStorageError::new(err.into()));
                    }
                    warn!(
                        attempt,
                        reason,
                        error = %err,
                        "retrying transaction event persist"
                    );
                    Metrics::transaction_events_persist_retries(reason).increment(1);
                    tokio::time::sleep(StdDuration::from_millis(25 * u64::from(attempt))).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Returns events for one transaction hash sorted by event time.
    pub async fn events_by_transaction_hash(
        &self,
        tx_hash: &str,
        limit: i64,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(limit);
        let lookup_keys = hex_lookup_keys(tx_hash);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE tx_hash = ANY($1) \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(&lookup_keys)
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
        let lookup_keys = hex_lookup_keys(block_hash);
        let rows = sqlx::query(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE block_hash = ANY($1) \
             ORDER BY event_time ASC, ingested_at ASC, event_id ASC \
             LIMIT $2",
        )
        .bind(&lookup_keys)
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
    ///
    /// Optional filters are omitted from SQL when unset so Postgres can use
    /// `transaction_events_rejected_event_time_idx` for a bounded `LIMIT`
    /// list. The previous `($1 IS NULL OR ...)` shape forced a heap scan on
    /// large journals and missed the Internal Explorer 3s timeout. `event_id`
    /// is a tie-break only; `ingested_at` is not in `ORDER BY` so the planner
    /// can keep the partial index.
    pub async fn rejected_transaction_events(
        &self,
        query: RejectedTransactionEventQuery,
    ) -> Result<Vec<TransactionEventRecord>> {
        let limit = normalize_limit(query.limit.unwrap_or(DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT));
        let from_block = query.from_block.map(i64::try_from).transpose()?;
        let to_block = query.to_block.map(i64::try_from).transpose()?;

        let mut query_builder = QueryBuilder::new(
            "SELECT event_id, schema_version, event_time, ingested_at, producer, event_type, \
             network, tx_hash, block_hash, block_number, payload_id, request_id, data \
             FROM transaction_events \
             WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED', 'BUILDER_EXPIRED')",
        );
        if let Some(from_block) = from_block {
            query_builder.push(" AND block_number >= ").push_bind(from_block);
        }
        if let Some(to_block) = to_block {
            query_builder.push(" AND block_number <= ").push_bind(to_block);
        }
        if let Some(from_time) = query.from_time {
            query_builder.push(" AND event_time >= ").push_bind(from_time);
        }
        if let Some(to_time) = query.to_time {
            query_builder.push(" AND event_time < ").push_bind(to_time);
        }
        // event_id is a tie-break only. The partial index is (event_type, event_time DESC);
        // LIMIT lists still use that leading column. ingested_at is omitted so the
        // planner does not drop the index for a three-column sort.
        query_builder.push(" ORDER BY event_time DESC, event_id DESC LIMIT ").push_bind(limit);

        let rows = query_builder.build().fetch_all(&self.pool).await?;
        rows.into_iter().map(record_from_row).collect()
    }
}

fn normalize_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_TRANSACTION_EVENT_QUERY_LIMIT)
}

/// Lookup keys for hex join columns stored as text.
///
/// Ingest writes `0x` + lowercase via `{hash:#x}`. Readers may send mixed
/// case, missing `0x`, or the original string; exact `ANY()` matches keep
/// the btree index while covering those variants.
fn hex_lookup_keys(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let mut keys = Vec::with_capacity(5);
    if !trimmed.is_empty() {
        keys.push(trimmed.to_string());
    }

    let hex = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")).unwrap_or(trimmed);
    if hex.is_empty() || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return keys;
    }

    let prefixed = format!("0x{}", hex.to_ascii_lowercase());
    if !keys.iter().any(|key| key == &prefixed) {
        keys.push(prefixed);
    }
    let prefixed_upper = format!("0x{}", hex.to_ascii_uppercase());
    if !keys.iter().any(|key| key == &prefixed_upper) {
        keys.push(prefixed_upper);
    }
    let bare = hex.to_ascii_lowercase();
    if !keys.iter().any(|key| key == &bare) {
        keys.push(bare);
    }
    let bare_upper = hex.to_ascii_uppercase();
    if !keys.iter().any(|key| key == &bare_upper) {
        keys.push(bare_upper);
    }
    keys
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

fn persist_retry_reason(err: &sqlx::Error) -> Option<&'static str> {
    let sqlx::Error::Database(database) = err else {
        return None;
    };
    persist_retry_sqlstate(database.code().as_deref()?)
}

fn persist_retry_sqlstate(code: &str) -> Option<&'static str> {
    match code {
        "40P01" => Some("deadlock"),
        "40001" => Some("serialization"),
        "55P03" => Some("lock_timeout"),
        _ => None,
    }
}

fn is_statement_timeout(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(database) if database.code().as_deref() == Some("57014"))
}

fn record_expire_statement_timeout(
    err: &sqlx::Error,
    class: TransactionEventRetentionClass,
    attempted_batch_limit: u32,
) {
    warn!(
        error = %err,
        retention_class = class.as_str(),
        attempted_batch_limit,
        "transaction event expire statement timed out"
    );
    Metrics::transaction_events_expire_statement_timeouts(class.as_str()).increment(1);
}

/// Next DELETE LIMIT after timeouts (`hi`) and full successes (`lo`).
fn expire_batch_limit(configured: u32, lo: u32, hi: Option<u32>, needs_limit_one: bool) -> u32 {
    if needs_limit_one {
        return 1;
    }
    let configured = configured.max(1);
    let Some(hi) = hi else {
        return configured;
    };
    let lo = lo.max(1).min(configured);
    let hi = hi.max(1);
    if lo.saturating_mul(2) >= hi {
        return lo;
    }
    let mid = lo.saturating_add(hi) / 2;
    mid.clamp(lo, configured).min(hi.saturating_sub(1)).max(1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetentionScanCursor {
    event_type: String,
    ingested_at: DateTime<Utc>,
    event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetentionClassProgress {
    scan_cutoff: DateTime<Utc>,
    cursor: Option<RetentionScanCursor>,
    limit_lo: u32,
    limit_hi: Option<u32>,
    needs_limit_one: bool,
    cycle_started_at: DateTime<Utc>,
    batches_this_cycle: u32,
}

impl RetentionClassProgress {
    fn new_cycle(
        now: DateTime<Utc>,
        config: &TransactionEventRetentionConfig,
        class: TransactionEventRetentionClass,
    ) -> Self {
        Self {
            scan_cutoff: now - Duration::days(i64::from(config.class_days(class))),
            cursor: None,
            limit_lo: 0,
            limit_hi: None,
            needs_limit_one: false,
            cycle_started_at: now,
            batches_this_cycle: 0,
        }
    }

    fn cycle_age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.cycle_started_at).num_seconds()
    }

    fn should_end_cycle(&self, now: DateTime<Utc>, interval_secs: u64) -> bool {
        if self.needs_limit_one || self.batches_this_cycle == 0 {
            return false;
        }
        let Ok(interval_secs) = i64::try_from(interval_secs) else {
            return false;
        };
        (now - self.cycle_started_at) >= Duration::seconds(interval_secs)
    }
}

enum ExpireBatchOutcome {
    Deleted { selected: u64, cursor: Option<RetentionScanCursor> },
    StatementTimeout,
}

async fn delete_expired_event_batch(
    conn: &mut sqlx::PgConnection,
    event_types: &[String],
    progress: &RetentionClassProgress,
    batch_limit: u32,
    class: TransactionEventRetentionClass,
    config: &TransactionEventRetentionConfig,
) -> Result<ExpireBatchOutcome> {
    let cutoff = progress.scan_cutoff;
    let timeout_sql = config.statement_timeout_sql();
    let mut attempt = 1u32;
    loop {
        let mut tx = conn.begin().await?;
        sqlx::query(&timeout_sql).execute(&mut *tx).await?;
        let should_sleep = class == TransactionEventRetentionClass::Hot
            && config.test_hot_sleep_ms.is_some()
            && config.test_hot_sleep_min_limit.is_none_or(|min_limit| batch_limit >= min_limit);
        if should_sleep && let Some(sleep_ms) = config.test_hot_sleep_ms {
            let sleep_secs = sleep_ms as f64 / 1_000.0;
            if let Err(err) =
                sqlx::query("SELECT pg_sleep($1)").bind(sleep_secs).execute(&mut *tx).await
            {
                let _ = tx.rollback().await;
                if is_statement_timeout(&err) {
                    record_expire_statement_timeout(&err, class, batch_limit);
                    return Ok(ExpireBatchOutcome::StatementTimeout);
                }
                return Err(err.into());
            }
        }
        // Delete by ctid via Tid Scan so the batch is not a second primary-key
        // lookup. ANY(ARRAY(ctid)) keeps that plan as catch-up shrinks the heap;
        // USING ctid can seq-scan. FOR UPDATE SKIP LOCKED skips rows held by a
        // concurrent writer instead of waiting. MATERIALIZED so the keyset
        // cursor is read from the selected set, not a rewritten join. The
        // resume key is the smallest selected tuple (last in DESC order) so
        // the next `WHERE key < cursor` scan does not walk just-deleted dead
        // index entries.
        let result = sqlx::query(
            "WITH doomed AS MATERIALIZED ( \
                SELECT ctid, event_type, ingested_at, event_id \
                FROM transaction_events \
                WHERE event_type = ANY($1) \
                  AND ingested_at < $2 \
                  AND ( \
                    $3::text IS NULL \
                    OR (event_type, ingested_at, event_id) < ($3, $4, $5) \
                  ) \
                ORDER BY event_type DESC, ingested_at DESC, event_id DESC \
                FOR UPDATE SKIP LOCKED \
                LIMIT $6 \
             ), \
             deleted AS ( \
                DELETE FROM transaction_events \
                WHERE ctid = ANY (ARRAY(SELECT ctid FROM doomed)::tid[]) \
                RETURNING ctid \
             ) \
             SELECT \
                (SELECT COUNT(*) FROM doomed)::bigint, \
                (SELECT COUNT(*) FROM deleted)::bigint, \
                (SELECT event_type FROM doomed \
                 ORDER BY event_type ASC, ingested_at ASC, event_id ASC LIMIT 1), \
                (SELECT ingested_at FROM doomed \
                 ORDER BY event_type ASC, ingested_at ASC, event_id ASC LIMIT 1), \
                (SELECT event_id FROM doomed \
                 ORDER BY event_type ASC, ingested_at ASC, event_id ASC LIMIT 1)",
        )
        .bind(event_types)
        .bind(cutoff)
        .bind(progress.cursor.as_ref().map(|cursor| cursor.event_type.as_str()))
        .bind(progress.cursor.as_ref().map(|cursor| cursor.ingested_at))
        .bind(progress.cursor.as_ref().map(|cursor| cursor.event_id.as_str()))
        .bind(i64::from(batch_limit))
        .fetch_one(&mut *tx)
        .await;
        match result {
            Ok(row) => {
                let selected: i64 = row.try_get(0)?;
                let deleted: i64 = row.try_get(1)?;
                if selected != deleted {
                    let _ = tx.rollback().await;
                    anyhow::bail!(
                        "transaction event expire selected {selected} rows but deleted {deleted}"
                    );
                }
                let cursor = match (
                    row.try_get::<Option<String>, _>(2)?,
                    row.try_get::<Option<DateTime<Utc>>, _>(3)?,
                    row.try_get::<Option<String>, _>(4)?,
                ) {
                    (Some(event_type), Some(ingested_at), Some(event_id)) => {
                        Some(RetentionScanCursor { event_type, ingested_at, event_id })
                    }
                    _ => None,
                };
                tx.commit().await?;
                return Ok(ExpireBatchOutcome::Deleted {
                    selected: u64::try_from(selected).unwrap_or(0),
                    cursor,
                });
            }
            Err(err) => {
                let _ = tx.rollback().await;
                if is_statement_timeout(&err) {
                    record_expire_statement_timeout(&err, class, batch_limit);
                    return Ok(ExpireBatchOutcome::StatementTimeout);
                }
                let Some(reason) = persist_retry_reason(&err) else {
                    return Err(err.into());
                };
                if attempt >= TRANSACTION_EVENT_DB_MAX_ATTEMPTS {
                    return Err(err.into());
                }
                warn!(
                    attempt,
                    reason,
                    error = %err,
                    "retrying transaction event expire"
                );
                Metrics::transaction_events_persist_retries(reason).increment(1);
                tokio::time::sleep(StdDuration::from_millis(25 * u64::from(attempt))).await;
                attempt += 1;
            }
        }
    }
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

    #[test]
    fn classifies_transaction_event_retention() {
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::ProxyBackendSuccess
            ),
            TransactionEventRetentionClass::Hot
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::IngressReceived),
            TransactionEventRetentionClass::Warm
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::TxpoolSendRawTransaction
            ),
            TransactionEventRetentionClass::Warm
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::TxpoolSendRawTransactionValidity
            ),
            TransactionEventRetentionClass::Warm
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::SimulationFailed),
            TransactionEventRetentionClass::Cold
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::BuilderRejected),
            TransactionEventRetentionClass::Hot
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::BuilderDeferred),
            TransactionEventRetentionClass::Hot
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::BuilderExpired),
            TransactionEventRetentionClass::Hot
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::BuilderPayloadFinalized
            ),
            TransactionEventRetentionClass::Cold
        );
    }

    #[test]
    fn retention_classes_partition_all_event_types() {
        let mut seen = std::collections::HashSet::new();
        for event_type in TransactionEventType::all() {
            assert!(seen.insert(event_type), "event type {event_type} appeared more than once");
            let class = TransactionEventRetentionClass::for_event_type(event_type);
            assert!(class.event_types().any(|candidate| candidate == event_type));
        }
    }

    #[test]
    fn validates_transaction_event_retention_bounds() {
        assert!(TransactionEventRetentionConfig::default().validate().is_ok());
        assert!(
            TransactionEventRetentionConfig { hot_days: 0, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig {
                hot_days: 14,
                warm_days: 7,
                cold_days: 30,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { delete_batch_size: 0, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { max_batches: 0, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { statement_timeout_ms: 0, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { statement_timeout_ms: 300_001, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { interval_secs: 0, ..Default::default() }
                .validate()
                .is_err()
        );
    }

    #[test]
    fn bisects_expire_batch_limit_below_the_timed_out_size() {
        assert_eq!(expire_batch_limit(10_000, 0, None, false), 10_000);
        assert_eq!(expire_batch_limit(10_000, 1, Some(10_000), true), 1);
        assert_eq!(expire_batch_limit(10_000, 1, Some(10_000), false), 5_000);
        assert_eq!(expire_batch_limit(10_000, 5_000, Some(10_000), false), 5_000);
        assert_eq!(expire_batch_limit(10_000, 1, Some(5_000), false), 2_500);
        assert_eq!(expire_batch_limit(10_000, 2_500, Some(5_000), false), 2_500);
        assert_eq!(expire_batch_limit(10_000, 1, Some(2), false), 1);
    }

    #[test]
    fn time_boxes_expire_cycle_after_the_first_returned_batch() {
        let now = Utc::now();
        let mut progress = RetentionClassProgress::new_cycle(
            now - Duration::seconds(10),
            &TransactionEventRetentionConfig::default(),
            TransactionEventRetentionClass::Hot,
        );
        assert!(
            !progress.should_end_cycle(now, 1),
            "a cycle with no returned batch must still run once"
        );
        progress.batches_this_cycle = 1;
        progress.needs_limit_one = true;
        assert!(
            !progress.should_end_cycle(now, 1),
            "a queued LIMIT 1 after timeout must run before the cycle restarts"
        );
        progress.needs_limit_one = false;
        assert!(progress.should_end_cycle(now, 1));
        assert!(!progress.should_end_cycle(now, 3_600));
    }

    #[test]
    fn classifies_retryable_postgres_sqlstates() {
        assert_eq!(persist_retry_sqlstate("40P01"), Some("deadlock"));
        assert_eq!(persist_retry_sqlstate("40001"), Some("serialization"));
        assert_eq!(persist_retry_sqlstate("55P03"), Some("lock_timeout"));
        assert_eq!(persist_retry_sqlstate("23505"), None);
        assert_eq!(persist_retry_sqlstate("57014"), None);
        assert_eq!(persist_retry_reason(&sqlx::Error::PoolTimedOut), None);
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
    async fn accepts_at_lifecycle_event_types() {
        let state = state(Arc::new(FakeSink::default()));
        let mut admission = event("at-admission");
        admission["producer"] = json!("base-reth-node");
        admission["event_type"] = json!("TXPOOL_SEND_RAW_TRANSACTION_VALIDITY");
        admission["data"] = json!({
            "rpc_method": "base_sendRawTransactionValidity",
            "validity_predicates": [{
                "type": "block_number",
                "params": { "op": ">=", "value": "0x64" }
            }]
        });
        let mut deferred = event("at-deferred");
        deferred["event_type"] = json!("BUILDER_DEFERRED");
        deferred["data"] = json!({ "defer_reason": "validity_predicate_not_satisfied" });
        let mut expired = event("at-expired");
        expired["event_type"] = json!("BUILDER_EXPIRED");
        expired["data"] = json!({ "expire_reason": "validity_predicate_expired" });

        let (status, Json(response)) =
            ingest_transaction_event_batch(&state, ndjson(vec![admission, deferred, expired]))
                .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, TransactionEventBatchStatus::Accepted);
        assert_eq!(response.accepted, 3);
        assert_eq!(response.rejected, 0);
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::TxpoolSendRawTransactionValidity
            ),
            TransactionEventRetentionClass::for_event_type(
                TransactionEventType::TxpoolSendRawTransaction
            )
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::BuilderDeferred),
            TransactionEventRetentionClass::Hot
        );
        assert_eq!(
            TransactionEventRetentionClass::for_event_type(TransactionEventType::BuilderExpired),
            TransactionEventRetentionClass::Hot
        );
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

    #[test]
    fn hex_lookup_keys_cover_prefixed_mixed_case_and_bare() {
        let keys = hex_lookup_keys("0xAa");
        assert_eq!(
            keys,
            vec![
                "0xAa".to_string(),
                "0xaa".to_string(),
                "0xAA".to_string(),
                "aa".to_string(),
                "AA".to_string(),
            ]
        );
    }

    #[test]
    fn hex_lookup_keys_keep_non_hex_input_as_exact_match() {
        assert_eq!(hex_lookup_keys("not-a-hash"), vec!["not-a-hash".to_string()]);
        assert!(hex_lookup_keys("   ").is_empty());
    }
}
