//! Metrics for audit operations including event reads, S3 writes, and event processing.

base_common_observability_metrics::define_metrics! {
    tips_audit
    #[describe("Transaction observability events received over HTTP")]
    transaction_events_received: counter,
    #[describe("Transaction observability events newly persisted to Postgres")]
    transaction_events_persisted: counter,
    #[describe("Transaction observability events skipped as duplicates")]
    transaction_events_duplicate: counter,
    #[describe("Transaction observability events rejected before persistence")]
    transaction_events_rejected: counter,
    #[describe("Transaction observability event validation failures")]
    transaction_events_validation_failures: counter,
    #[describe("Transaction observability event database persistence failures")]
    transaction_events_database_failures: counter,
    #[describe("Retryable Postgres lock errors on transaction observability INSERT or expire DELETE")]
    #[label(name = "reason", default = ["deadlock", "serialization", "lock_timeout"])]
    transaction_events_persist_retries: counter,
    #[describe("Transaction observability HTTP ingest batch size")]
    transaction_event_batch_size: histogram,
    #[describe("Duration of transaction observability Postgres batch writes")]
    transaction_event_batch_write_duration: histogram,
    #[describe("Expired transaction observability rows deleted from Postgres")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_events_expired: counter,
    #[describe("Transaction observability expire batches canceled by statement timeout")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_events_expire_statement_timeouts: counter,
    #[describe("Effective transaction observability expire DELETE LIMIT")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_retention_effective_batch_limit: gauge,
    #[describe("Transaction observability expire scan cycles ended")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    #[label(name = "reason", default = ["interval", "exhausted"])]
    transaction_event_retention_cycles_ended: counter,
    #[describe("Transaction observability retention delete failures")]
    transaction_event_retention_failures: counter,
}
