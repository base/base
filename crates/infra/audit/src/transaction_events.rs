//! HTTP ingest path for transaction observability events.

use std::{
    collections::{BTreeSet, HashSet},
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
use chrono::{DateTime, Duration, NaiveDate, Utc};
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

/// Attempts per INSERT chunk, including the first try.
const TRANSACTION_EVENT_DB_MAX_ATTEMPTS: u32 = 3;

/// Default days to keep high-volume proxy and builder-decision events.
pub const DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS: u32 = 3;
/// Default days to keep ingress, simulation-success, and txpool-forward events.
pub const DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS: u32 = 7;
/// Default days to keep failures, drops, inclusion, and flashblock events.
pub const DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS: u32 = 30;
/// Maximum configurable retention days for any class.
///
/// Bounds the number of day partitions per class.
pub const MAX_TRANSACTION_EVENT_RETENTION_DAYS: u32 = 90;
/// Default seconds between partition maintenance passes.
pub const DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS: u64 = 3_600;
/// Default Postgres `lock_timeout` for one partition create, detach, or drop.
///
/// Detach takes an ACCESS EXCLUSIVE lock on the class partition, which queues
/// inserts for that class behind it. A short timeout keeps a blocked detach
/// from stalling ingest; the next pass retries it.
pub const DEFAULT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS: u64 = 5_000;
/// Whole UTC days of partitions kept ahead of the current day.
///
/// Ingest keeps working this long if partition maintenance stops.
pub const TRANSACTION_EVENT_PARTITION_DAYS_AHEAD: u32 = 3;
/// Maximum amount an event's `event_time` may be ahead of the server clock.
///
/// Later events are rejected instead of failing the batch with a missing
/// partition.
pub const MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS: i64 = 3_600;
/// Extra age a day partition must reach past its retention window before it
/// is dropped.
///
/// Ingest admits events up to exactly the retention window, so the grace
/// period keeps an in-flight insert from racing its partition's drop.
const TRANSACTION_EVENT_PARTITION_DROP_GRACE_SECS: i64 = 3_600;
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

/// Retention class used to pick an event's partition and retention window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransactionEventRetentionClass {
    /// High-volume success-path and repeated builder-decision events.
    Hot,
    /// Ordinary arrival, simulation-success, and forwarding events.
    Warm,
    /// High-value inclusion, finalization, rejection, and drop events.
    Cold,
}

impl TransactionEventRetentionClass {
    /// Every retention class, in maintenance order.
    pub const ALL: [Self; 3] = [Self::Hot, Self::Warm, Self::Cold];

    /// Stable lowercase value used as a bounded metric label and as the
    /// Postgres `retention_class` partition value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }

    /// Parses the Postgres `retention_class` value.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == label)
    }

    /// Classifies a transaction event at ingest.
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

    #[cfg(test)]
    fn event_types(self) -> impl Iterator<Item = TransactionEventType> {
        TransactionEventType::all()
            .filter(move |event_type| Self::for_event_type(*event_type) == self)
    }
}

/// Configuration for transaction event retention and partition maintenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionEventRetentionConfig {
    /// Days to keep hot event types after `event_time`.
    pub hot_days: u32,
    /// Days to keep warm event types after `event_time`.
    pub warm_days: u32,
    /// Days to keep cold event types after `event_time`.
    pub cold_days: u32,
    /// Postgres `lock_timeout` for one partition DDL statement, in
    /// milliseconds.
    pub partition_lock_timeout_ms: u64,
    /// Seconds between partition maintenance passes.
    pub interval_secs: u64,
}

impl Default for TransactionEventRetentionConfig {
    fn default() -> Self {
        Self {
            hot_days: DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS,
            warm_days: DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS,
            cold_days: DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS,
            partition_lock_timeout_ms: DEFAULT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS,
            interval_secs: DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
        }
    }
}

impl TransactionEventRetentionConfig {
    /// Validates retention bounds before ingest or partition maintenance.
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
            self.cold_days <= MAX_TRANSACTION_EVENT_RETENTION_DAYS,
            "transaction event cold retention days must be at most {MAX_TRANSACTION_EVENT_RETENTION_DAYS}"
        );
        anyhow::ensure!(
            (1..=60_000).contains(&self.partition_lock_timeout_ms),
            "transaction event partition lock timeout must be between 1ms and 60000ms"
        );
        anyhow::ensure!(
            (1..=604_800).contains(&self.interval_secs),
            "transaction event retention interval must be between 1 and 604800 seconds"
        );
        Ok(self)
    }

    fn partition_lock_timeout_sql(self) -> String {
        format!("SET LOCAL lock_timeout = '{}ms'", self.partition_lock_timeout_ms)
    }

    const fn class_days(self, class: TransactionEventRetentionClass) -> u32 {
        match class {
            TransactionEventRetentionClass::Hot => self.hot_days,
            TransactionEventRetentionClass::Warm => self.warm_days,
            TransactionEventRetentionClass::Cold => self.cold_days,
        }
    }

    fn retention_window(self, class: TransactionEventRetentionClass) -> Duration {
        Duration::days(i64::from(self.class_days(class)))
    }

    /// Checks that ingest can store an event with this `event_time` now.
    ///
    /// Events older than their class's retention window would be dropped with
    /// their partition anyway, and events too far in the future have no
    /// partition yet. Rejecting both keeps one bad timestamp from failing a
    /// whole insert batch.
    fn admit_event_time(
        self,
        class: TransactionEventRetentionClass,
        event_time: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), EventTimeRejection> {
        if event_time < now - self.retention_window(class) {
            return Err(EventTimeRejection::Expired);
        }
        if event_time >= now + Duration::seconds(MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS) {
            return Err(EventTimeRejection::Future);
        }
        Ok(())
    }
}

/// Why ingest refused an event's `event_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventTimeRejection {
    /// Older than the event's retention window.
    Expired,
    /// Further ahead of the server clock than the allowed skew.
    Future,
}

impl EventTimeRejection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Future => "future",
        }
    }

    fn reason(self, class: TransactionEventRetentionClass) -> String {
        match self {
            Self::Expired => {
                format!("event_time is older than the {} retention window", class.as_str())
            }
            Self::Future => format!(
                "event_time is more than {MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS} seconds in the future"
            ),
        }
    }
}

/// Result of one locked partition maintenance pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionEventRetentionOutcome {
    /// Whether this replica held the retention lock and ran the pass.
    pub lock_acquired: bool,
    /// Day partitions created in this pass.
    pub partitions_created: u64,
    /// Expired day partitions dropped in this pass.
    pub partitions_dropped: u64,
    /// Partition DDL statements skipped after hitting the lock timeout.
    pub lock_timeouts: u64,
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
const REQUIRED_TRANSACTION_EVENT_MIGRATION_DESCRIPTION: &str = "transaction events partitioned";
static TRANSACTION_EVENT_MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Required sqlx migration version for transaction event storage.
fn required_transaction_event_migration_version() -> Result<i64, &'static str> {
    let mut matching_migrations = TRANSACTION_EVENT_MIGRATOR.iter().filter(|migration| {
        migration.description.as_ref() == REQUIRED_TRANSACTION_EVENT_MIGRATION_DESCRIPTION
    });
    let migration = matching_migrations.next().ok_or(
        "transaction event migration 005_transaction_events_partitioned.sql must be embedded in audit migrator",
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
        "transaction-event Postgres schema is not ready: required sqlx migration version {required_version} for 005_transaction_events_partitioned.sql has not been applied successfully; run `audit-archiver migrate up` or the audit migration WorkflowTemplate before enabling TIPS_AUDIT_POSTGRES_URL"
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
        "transaction-event Postgres schema is not ready: runtime role cannot query public.transaction_events; verify audit_archiver privileges from 005_transaction_events_partitioned.sql"
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

    /// Checks whether the sink can store a validated event right now.
    ///
    /// Returns a rejection reason for events the sink would refuse, such as an
    /// `event_time` outside the partitions it keeps. The default admits
    /// everything.
    fn admit_event(
        &self,
        _event: &TransactionEvent,
        _now: DateTime<Utc>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

/// Postgres-backed transaction event sink.
#[derive(Debug, Clone)]
pub struct PgTransactionEventSink {
    pool: PgPool,
    retention_pool: PgPool,
    retention: TransactionEventRetentionConfig,
}

impl PgTransactionEventSink {
    /// Required sqlx migration version for transaction event storage.
    pub fn required_migration_version() -> Result<i64, &'static str> {
        required_transaction_event_migration_version()
    }

    /// Connects to Postgres without running migrations.
    ///
    /// The ingest pool is used for persist, RPC reads, and `/readyz`.
    /// Partition maintenance uses a dedicated one-connection pool with a short
    /// acquire timeout so lock losers do not occupy ingest connections. The
    /// sink starts with the default retention config; see
    /// [`Self::with_retention_config`].
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        let max_connections = max_connections.max(1);
        let pool =
            PgPoolOptions::new().max_connections(max_connections).connect(database_url).await?;
        let retention_pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(TRANSACTION_EVENT_RETENTION_ACQUIRE_TIMEOUT)
            .connect(database_url)
            .await?;
        Ok(Self::new_with_retention_pool(pool, retention_pool))
    }

    /// Runs pending Postgres migrations.
    pub async fn migrate(database_url: &str) -> Result<()> {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        TRANSACTION_EVENT_MIGRATOR.run(&pool).await?;
        Ok(())
    }

    /// Creates a sink from an existing ingest pool. Retention uses the same pool.
    pub fn new(pool: PgPool) -> Self {
        Self::new_with_retention_pool(pool.clone(), pool)
    }

    /// Creates a sink with a dedicated retention pool.
    pub fn new_with_retention_pool(pool: PgPool, retention_pool: PgPool) -> Self {
        Self { pool, retention_pool, retention: TransactionEventRetentionConfig::default() }
    }

    /// Sets the retention windows used for ingest admission and partition
    /// maintenance.
    pub fn with_retention_config(
        mut self,
        retention: TransactionEventRetentionConfig,
    ) -> Result<Self> {
        self.retention = retention.validate()?;
        Ok(self)
    }

    /// Retention windows used for ingest admission and partition maintenance.
    pub const fn retention_config(&self) -> TransactionEventRetentionConfig {
        self.retention
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

    /// Creates upcoming day partitions and drops expired ones.
    ///
    /// Uses a session advisory lock so only one replica changes partitions at
    /// a time. Returns an outcome with `lock_acquired = false` when another
    /// replica holds the lock or the retention pool times out waiting for a
    /// connection. The lock is released after the pass, including when a
    /// statement fails. If unlock itself fails, the connection is detached so
    /// the session lock cannot leak onto a reused pool connection.
    ///
    /// Each class keeps day partitions from the start of its retention window
    /// through [`TRANSACTION_EVENT_PARTITION_DAYS_AHEAD`] days after today. A
    /// day is dropped once all of it is older than the retention window plus
    /// a one-hour grace period. Each DDL statement runs in its own transaction
    /// under the configured `lock_timeout`; a statement that times out is
    /// skipped and retried on the next pass.
    pub async fn maintain_partitions(&self) -> Result<TransactionEventRetentionOutcome> {
        self.maintain_partitions_at(Utc::now()).await
    }

    /// Runs [`Self::maintain_partitions`] as if the current time were `now`.
    ///
    /// Tests use this to move the retention window without waiting for days.
    #[doc(hidden)]
    pub async fn maintain_partitions_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<TransactionEventRetentionOutcome> {
        let config = self.retention;
        let Some(mut lock) = RetentionAdvisoryLock::try_acquire(&self.retention_pool).await? else {
            return Ok(TransactionEventRetentionOutcome::default());
        };

        let outcome = async {
            let existing = list_day_partitions(lock.conn()).await?;
            let plan = plan_partition_maintenance(now, config, &existing);
            let lock_timeout_sql = config.partition_lock_timeout_sql();
            let mut outcome =
                TransactionEventRetentionOutcome { lock_acquired: true, ..Default::default() };
            let mut attached: BTreeSet<DayPartition> = existing
                .iter()
                .filter(|partition| partition.attached)
                .map(|partition| partition.partition)
                .collect();

            // Leftovers from a pass that detached but failed to drop. Drop them
            // first so a same-named create cannot collide with them.
            for partition in plan.drop_detached {
                if run_partition_ddl(
                    lock.conn(),
                    &lock_timeout_sql,
                    PartitionDdl::DropDetached,
                    partition,
                    &mut outcome,
                )
                .await?
                {
                    outcome.partitions_dropped += 1;
                    Metrics::transaction_event_partitions_dropped(partition.class.as_str())
                        .increment(1);
                }
            }

            for partition in plan.create {
                if run_partition_ddl(
                    lock.conn(),
                    &lock_timeout_sql,
                    PartitionDdl::Create,
                    partition,
                    &mut outcome,
                )
                .await?
                {
                    outcome.partitions_created += 1;
                    Metrics::transaction_event_partitions_created(partition.class.as_str())
                        .increment(1);
                    attached.insert(partition);
                }
            }

            for partition in plan.detach {
                let detached = run_partition_ddl(
                    lock.conn(),
                    &lock_timeout_sql,
                    PartitionDdl::Detach,
                    partition,
                    &mut outcome,
                )
                .await?;
                if !detached {
                    continue;
                }
                attached.remove(&partition);
                if run_partition_ddl(
                    lock.conn(),
                    &lock_timeout_sql,
                    PartitionDdl::DropDetached,
                    partition,
                    &mut outcome,
                )
                .await?
                {
                    outcome.partitions_dropped += 1;
                    Metrics::transaction_event_partitions_dropped(partition.class.as_str())
                        .increment(1);
                }
            }

            for class in TransactionEventRetentionClass::ALL {
                Metrics::transaction_event_partition_horizon_seconds(class.as_str())
                    .set(partition_horizon_secs(now, class, &attached));
            }
            Ok(outcome)
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
                 (event_id, schema_version, event_time, retention_class, producer, event_type, \
                  network, tx_hash, block_hash, block_number, payload_id, request_id, data) ",
            );
            query_builder.push_values(
                ordered.iter().copied().zip(block_numbers.iter().copied()),
                |mut row, (event, block_number)| {
                    let tx_hash = event.tx_hash.map(|hash| format!("{hash:#x}"));
                    let block_hash = event.block_hash.map(|hash| format!("{hash:#x}"));
                    let producer = event.producer.to_string();
                    let event_type = event.event_type.to_string();
                    let retention_class =
                        TransactionEventRetentionClass::for_event_type(event.event_type).as_str();
                    let data = Value::Object(event.data.clone());

                    row.push_bind(&event.event_id)
                        .push_bind(&event.schema_version)
                        .push_bind(event.event_time)
                        .push_bind(retention_class)
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
            // The partitioned primary key includes retention_class and event_time;
            // both are fixed per event, so retries still conflict.
            query_builder.push(
                " ON CONFLICT (event_id, retention_class, event_time) DO NOTHING RETURNING event_id",
            );

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

fn is_lock_timeout(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(database) if database.code().as_deref() == Some("55P03"))
}

/// Midnight UTC at the start of `day`.
const fn utc_midnight(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(chrono::NaiveTime::MIN).and_utc()
}

/// One UTC day partition of one retention class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DayPartition {
    class: TransactionEventRetentionClass,
    day: NaiveDate,
}

impl DayPartition {
    /// Table name used by the partition functions in migration 005.
    fn table_name(self) -> String {
        format!("transaction_events_{}_{}", self.class.as_str(), self.day.format("%Y%m%d"))
    }

    fn from_table_name(name: &str) -> Option<Self> {
        let (class, day) = name.strip_prefix("transaction_events_")?.split_once('_')?;
        if day.len() != 8 {
            return None;
        }
        Some(Self {
            class: TransactionEventRetentionClass::from_label(class)?,
            day: NaiveDate::parse_from_str(day, "%Y%m%d").ok()?,
        })
    }

    /// Exclusive upper bound of the partition's `event_time` range.
    fn end(self) -> DateTime<Utc> {
        utc_midnight(self.day + Duration::days(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExistingPartition {
    partition: DayPartition,
    /// Whether the table is still attached to its class partition.
    attached: bool,
}

/// Partition DDL for one maintenance pass, in execution order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PartitionPlan {
    /// Tables left detached by an earlier pass that failed before dropping.
    drop_detached: Vec<DayPartition>,
    /// Missing days in each class's retention window and look-ahead.
    create: Vec<DayPartition>,
    /// Attached days entirely older than the retention window plus grace.
    detach: Vec<DayPartition>,
}

/// Plans which day partitions to create and drop at `now`.
fn plan_partition_maintenance(
    now: DateTime<Utc>,
    config: TransactionEventRetentionConfig,
    existing: &[ExistingPartition],
) -> PartitionPlan {
    let attached: BTreeSet<DayPartition> = existing
        .iter()
        .filter(|partition| partition.attached)
        .map(|partition| partition.partition)
        .collect();
    let mut plan = PartitionPlan {
        drop_detached: existing
            .iter()
            .filter(|partition| !partition.attached)
            .map(|partition| partition.partition)
            .collect(),
        ..Default::default()
    };

    let last_day = (now + Duration::seconds(MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS)).date_naive()
        + Duration::days(i64::from(TRANSACTION_EVENT_PARTITION_DAYS_AHEAD));
    for class in TransactionEventRetentionClass::ALL {
        let window = config.retention_window(class);
        let mut day = (now - window).date_naive();
        while day <= last_day {
            let partition = DayPartition { class, day };
            if !attached.contains(&partition) {
                plan.create.push(partition);
            }
            day += Duration::days(1);
        }

        let drop_before =
            now - window - Duration::seconds(TRANSACTION_EVENT_PARTITION_DROP_GRACE_SECS);
        plan.detach.extend(
            attached
                .iter()
                .filter(|partition| partition.class == class && partition.end() <= drop_before)
                .copied(),
        );
    }
    plan
}

/// Seconds until ingest for `class` would hit a missing partition.
///
/// Counts contiguous attached days starting with today. Zero when today's
/// partition is missing.
fn partition_horizon_secs(
    now: DateTime<Utc>,
    class: TransactionEventRetentionClass,
    attached: &BTreeSet<DayPartition>,
) -> f64 {
    let mut day = now.date_naive();
    if !attached.contains(&DayPartition { class, day }) {
        return 0.0;
    }
    while attached.contains(&DayPartition { class, day: day + Duration::days(1) }) {
        day += Duration::days(1);
    }
    let secs = (utc_midnight(day + Duration::days(1)) - now).num_seconds();
    f64::from(i32::try_from(secs).unwrap_or(i32::MAX))
}

async fn list_day_partitions(conn: &mut sqlx::PgConnection) -> Result<Vec<ExistingPartition>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.relname::text, c.relispartition \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' \
           AND c.relkind = 'r' \
           AND c.relname ~ '^transaction_events_(hot|warm|cold)_[0-9]{8}$'",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, attached)| {
            DayPartition::from_table_name(&name)
                .map(|partition| ExistingPartition { partition, attached })
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionDdl {
    Create,
    Detach,
    DropDetached,
}

impl PartitionDdl {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Detach => "detach",
            Self::DropDetached => "drop",
        }
    }

    const fn sql(self) -> &'static str {
        match self {
            Self::Create => "SELECT public.transaction_events_create_partition($1, $2)",
            Self::Detach => "SELECT public.transaction_events_detach_partition($1, $2)",
            Self::DropDetached => {
                "SELECT public.transaction_events_drop_detached_partition($1, $2)"
            }
        }
    }
}

/// Runs one partition function in its own transaction.
///
/// Returns whether the function changed anything. A lock timeout is counted
/// and reported as unchanged so the pass can continue; the next pass retries.
async fn run_partition_ddl(
    conn: &mut sqlx::PgConnection,
    lock_timeout_sql: &str,
    ddl: PartitionDdl,
    partition: DayPartition,
    outcome: &mut TransactionEventRetentionOutcome,
) -> Result<bool> {
    let mut tx = conn.begin().await?;
    sqlx::query(lock_timeout_sql).execute(&mut *tx).await?;
    let result: std::result::Result<bool, sqlx::Error> = sqlx::query_scalar(ddl.sql())
        .bind(partition.class.as_str())
        .bind(partition.day)
        .fetch_one(&mut *tx)
        .await;
    match result {
        Ok(changed) => {
            tx.commit().await?;
            Ok(changed)
        }
        Err(err) => {
            let _ = tx.rollback().await;
            if is_lock_timeout(&err) {
                warn!(
                    error = %err,
                    action = ddl.as_str(),
                    partition = %partition.table_name(),
                    "transaction event partition DDL hit lock timeout; retrying next pass"
                );
                outcome.lock_timeouts += 1;
                Metrics::transaction_event_partition_lock_timeouts(ddl.as_str()).increment(1);
                return Ok(false);
            }
            Err(anyhow::Error::new(err).context(format!(
                "failed to {} transaction event partition {}",
                ddl.as_str(),
                partition.table_name()
            )))
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

    fn admit_event(
        &self,
        event: &TransactionEvent,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), String> {
        let class = TransactionEventRetentionClass::for_event_type(event.event_type);
        self.retention.admit_event_time(class, event.event_time, now).map_err(|rejection| {
            Metrics::transaction_events_outside_retention_window(rejection.as_str()).increment(1);
            rejection.reason(class)
        })
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
    let now = Utc::now();
    for raw_event in events {
        let admitted = validate_transaction_event(raw_event, &state.config).and_then(|event| {
            match state.sink.admit_event(&event, now) {
                Ok(()) => Ok(event),
                Err(reason) => Err(ValidationRejection { event_id: Some(event.event_id), reason }),
            }
        });
        match admitted {
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

    /// Fake sink that applies the real retention-window admission check.
    #[derive(Debug, Default)]
    struct RetentionWindowSink {
        inner: FakeSink,
        retention: TransactionEventRetentionConfig,
    }

    #[async_trait]
    impl TransactionEventSink for RetentionWindowSink {
        async fn insert_events(
            &self,
            events: &[TransactionEvent],
        ) -> std::result::Result<TransactionEventInsertOutcome, TransactionEventStorageError>
        {
            self.inner.insert_events(events).await
        }

        fn admit_event(
            &self,
            event: &TransactionEvent,
            now: DateTime<Utc>,
        ) -> std::result::Result<(), String> {
            let class = TransactionEventRetentionClass::for_event_type(event.event_type);
            self.retention
                .admit_event_time(class, event.event_time, now)
                .map_err(|rejection| rejection.reason(class))
        }
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
    }

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    fn partition(class: TransactionEventRetentionClass, value: &str) -> DayPartition {
        DayPartition { class, day: day(value) }
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
            TransactionEventRetentionConfig {
                cold_days: MAX_TRANSACTION_EVENT_RETENTION_DAYS + 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            TransactionEventRetentionConfig { partition_lock_timeout_ms: 0, ..Default::default() }
                .validate()
                .is_err()
        );
        assert!(
            TransactionEventRetentionConfig {
                partition_lock_timeout_ms: 60_001,
                ..Default::default()
            }
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
    fn admits_event_times_inside_the_retention_window() {
        let config = TransactionEventRetentionConfig::default();
        let now = at("2026-09-23T12:00:00Z");
        let hot = TransactionEventRetentionClass::Hot;

        assert_eq!(config.admit_event_time(hot, now, now), Ok(()));
        assert_eq!(config.admit_event_time(hot, now - Duration::days(3), now), Ok(()));
        assert_eq!(
            config.admit_event_time(hot, now - Duration::days(3) - Duration::seconds(1), now),
            Err(EventTimeRejection::Expired)
        );
        assert_eq!(
            config.admit_event_time(
                TransactionEventRetentionClass::Cold,
                now - Duration::days(29),
                now
            ),
            Ok(()),
            "cold events keep the longer window"
        );
        assert_eq!(config.admit_event_time(hot, now + Duration::minutes(59), now), Ok(()));
        assert_eq!(
            config.admit_event_time(hot, now + Duration::hours(1), now),
            Err(EventTimeRejection::Future)
        );
    }

    #[test]
    fn round_trips_day_partition_table_names() {
        let hot = partition(TransactionEventRetentionClass::Hot, "2026-09-23");
        assert_eq!(hot.table_name(), "transaction_events_hot_20260923");
        assert_eq!(DayPartition::from_table_name(&hot.table_name()), Some(hot));
        assert_eq!(hot.end(), at("2026-09-24T00:00:00Z"));

        for name in [
            "transaction_events",
            "transaction_events_hot",
            "transaction_events_tepid_20260923",
            "transaction_events_hot_2026092",
            "transaction_events_hot_20261341",
        ] {
            assert_eq!(DayPartition::from_table_name(name), None, "{name}");
        }
    }

    #[test]
    fn plans_each_class_window_plus_days_ahead() {
        let config = TransactionEventRetentionConfig::default();
        let now = at("2026-09-23T12:00:00Z");

        let plan = plan_partition_maintenance(now, config, &[]);

        let first_and_last = |class| {
            let days: Vec<_> =
                plan.create.iter().filter(|p| p.class == class).map(|p| p.day).collect();
            (days.len(), days.first().copied(), days.last().copied())
        };
        assert_eq!(
            first_and_last(TransactionEventRetentionClass::Hot),
            (7, Some(day("2026-09-20")), Some(day("2026-09-26")))
        );
        assert_eq!(
            first_and_last(TransactionEventRetentionClass::Warm),
            (11, Some(day("2026-09-16")), Some(day("2026-09-26")))
        );
        assert_eq!(
            first_and_last(TransactionEventRetentionClass::Cold),
            (34, Some(day("2026-08-24")), Some(day("2026-09-26")))
        );
        assert!(plan.detach.is_empty());
        assert!(plan.drop_detached.is_empty());
    }

    #[test]
    fn plans_drops_after_the_window_and_grace_period() {
        let config = TransactionEventRetentionConfig::default();
        let hot = TransactionEventRetentionClass::Hot;
        let existing = [
            // Ends at the window start on Sep 20 00:00; dropped once the grace
            // period has also passed.
            ExistingPartition { partition: partition(hot, "2026-09-19"), attached: true },
            ExistingPartition { partition: partition(hot, "2026-09-20"), attached: true },
            ExistingPartition {
                partition: partition(TransactionEventRetentionClass::Cold, "2026-07-01"),
                attached: false,
            },
        ];

        let within_grace =
            plan_partition_maintenance(at("2026-09-23T00:30:00Z"), config, &existing);
        assert!(within_grace.detach.is_empty(), "grace period keeps the partition");

        let plan = plan_partition_maintenance(at("2026-09-23T12:00:00Z"), config, &existing);
        assert_eq!(plan.detach, vec![partition(hot, "2026-09-19")]);
        assert_eq!(
            plan.drop_detached,
            vec![partition(TransactionEventRetentionClass::Cold, "2026-07-01")]
        );
        assert!(
            !plan.create.contains(&partition(hot, "2026-09-20")),
            "attached partitions are not recreated"
        );
    }

    #[test]
    fn measures_contiguous_partition_horizon_from_today() {
        let hot = TransactionEventRetentionClass::Hot;
        let now = at("2026-09-23T12:00:00Z");
        let attached: BTreeSet<_> = ["2026-09-23", "2026-09-24", "2026-09-25", "2026-09-27"]
            .into_iter()
            .map(|value| partition(hot, value))
            .collect();

        // Sep 26 is missing, so ingest runs out at Sep 26 00:00.
        assert_eq!(partition_horizon_secs(now, hot, &attached), 2.5 * 86_400.0);
        assert_eq!(
            partition_horizon_secs(now, TransactionEventRetentionClass::Warm, &attached),
            0.0
        );
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
    async fn rejects_events_outside_the_retention_window() {
        let state = state(Arc::new(RetentionWindowSink::default()));
        let mut expired = event("expired-event");
        expired["event_time"] = json!(Utc::now() - Duration::days(4));
        let mut future = event("future-event");
        future["event_time"] = json!(Utc::now() + Duration::days(1));

        let (status, Json(response)) = ingest_transaction_event_batch(
            &state,
            ndjson(vec![event("fresh-event"), expired, future]),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, TransactionEventBatchStatus::Partial);
        assert_eq!(response.accepted, 1);
        assert_eq!(response.rejected, 2);
        let reasons: Vec<_> =
            response.results.iter().filter_map(|result| result.reason.as_deref()).collect();
        assert_eq!(
            reasons,
            vec![
                "event_time is older than the hot retention window",
                "event_time is more than 3600 seconds in the future"
            ]
        );
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
