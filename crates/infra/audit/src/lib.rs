#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod metrics;
pub use metrics::Metrics;

mod ingested_at_index;
pub use ingested_at_index::{TransactionEventIngestedAtIndex, index_transaction_event_partitions};

mod migration;
pub use migration::{AuditMigration, RequiredAuditWork};

mod migration_status;
pub use migration_status::{
    MigrationError, MigrationPhase, MigrationReporter, MigrationState, MigrationStatus,
};

mod migration_session;
pub use migration_session::{MigrationBackend, MigrationServer, MigrationSession};

mod migration_store;
pub use migration_store::{MigrationRecord, MigrationStore};

mod migration_cache;
pub use migration_cache::{MigrationCachePermit, MigrationCacheWrite, MigrationCacheWriter};

mod migration_durable;
pub use migration_durable::{MigrationDurable, MigrationDurableKey};

mod managed_migration;
pub use managed_migration::{ManagedMigration, ManagedMigrationConfig};

mod migration_http;
pub use migration_http::MigrationHttp;

mod rpc;
pub use rpc::{AuditArchiverApiServer, AuditArchiverRpc};

mod transaction_events;
pub use transaction_events::{
    DEFAULT_TRANSACTION_EVENT_BATCH_PATH, DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS,
    DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS, DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE,
    DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES, DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES,
    DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES,
    DEFAULT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS, DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT,
    DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
    DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS, MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS,
    MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE, MAX_TRANSACTION_EVENT_QUERY_LIMIT,
    MAX_TRANSACTION_EVENT_RETENTION_DAYS, MAX_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
    PgTransactionEventSink, RejectedTransactionEventQuery, TRANSACTION_EVENT_PARTITION_DAYS_AHEAD,
    TransactionEventBatchResponse, TransactionEventBatchStatus, TransactionEventIngestConfig,
    TransactionEventInsertOutcome, TransactionEventItemResult, TransactionEventItemStatus,
    TransactionEventRecord, TransactionEventRetentionClass, TransactionEventRetentionConfig,
    TransactionEventRetentionOutcome, TransactionEventSchemaReadinessError, TransactionEventSink,
    TransactionEventStorageError,
};
