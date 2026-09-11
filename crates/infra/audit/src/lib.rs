#![doc = include_str!("../README.md")]

mod metrics;
pub use metrics::Metrics;

mod rpc;
pub use rpc::{AuditArchiverApiServer, AuditArchiverRpc};

mod storage;
pub use storage::RejectedTransactionStore;

mod transaction_events;
pub use transaction_events::{
    DEFAULT_TRANSACTION_EVENT_BATCH_PATH, DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS,
    DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS, DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE,
    DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES, DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES,
    DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES, DEFAULT_TRANSACTION_EVENT_QUERY_LIMIT,
    DEFAULT_TRANSACTION_EVENT_RETENTION_BATCH_SIZE,
    DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
    DEFAULT_TRANSACTION_EVENT_RETENTION_MAX_BATCHES,
    DEFAULT_TRANSACTION_EVENT_RETENTION_STATEMENT_TIMEOUT_MS,
    DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS, MAX_TRANSACTION_EVENT_INSERT_BATCH_SIZE,
    MAX_TRANSACTION_EVENT_QUERY_LIMIT, PgTransactionEventSink, RejectedTransactionEventQuery,
    TransactionEventBatchResponse, TransactionEventBatchStatus, TransactionEventIngestConfig,
    TransactionEventInsertOutcome, TransactionEventItemResult, TransactionEventItemStatus,
    TransactionEventRecord, TransactionEventRetentionClass, TransactionEventRetentionConfig,
    TransactionEventRetentionOutcome, TransactionEventSchemaReadinessError, TransactionEventSink,
    TransactionEventStorageError,
};
