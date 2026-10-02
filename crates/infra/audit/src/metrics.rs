//! Metrics for Postgres-backed transaction event ingest and retention.

base_metrics::define_metrics! {
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
    #[describe("Retryable Postgres lock errors on transaction observability INSERT")]
    #[label(name = "reason", default = ["deadlock", "serialization", "lock_timeout"])]
    transaction_events_persist_retries: counter,
    #[describe("Transaction observability HTTP ingest batch size")]
    transaction_event_batch_size: histogram,
    #[describe("Duration of transaction observability Postgres batch writes")]
    transaction_event_batch_write_duration: histogram,
    #[describe("Transaction observability events rejected because event_time is outside the stored partitions")]
    #[label(name = "reason", default = ["expired", "future"])]
    transaction_events_outside_retention_window: counter,
    #[describe("Transaction observability day partitions created")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partitions_created: counter,
    #[describe("Expired transaction observability day partitions dropped")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partitions_dropped: counter,
    #[describe("Transaction observability partition DDL statements skipped after lock_timeout")]
    #[label(name = "action", default = ["create", "detach", "drop"])]
    transaction_event_partition_lock_timeouts: counter,
    #[describe("Seconds until transaction observability ingest would reach a missing day partition")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partition_horizon_seconds: gauge,
    #[describe("Transaction observability partition maintenance failures")]
    transaction_event_retention_failures: counter,
    #[describe("Managed migration result, independent of readiness")]
    #[label(name = "state", default = ["running", "succeeded", "failed", "stopped"])]
    migration_state: gauge,
    #[describe("Managed migration lifecycle phase")]
    #[label(name = "phase", default = ["starting", "waiting_for_lock", "schema", "reconciling", "validating", "idle", "stopping"])]
    migration_phase: gauge,
    #[describe("Embedded schema migrations committed and verified")]
    migration_schema_ready: gauge,
    #[describe("Migration supervisor and worker available")]
    migration_worker_available: gauge,
    #[describe("Every registered migration requirement validated")]
    migration_complete: gauge,
    #[describe("Owned migration attempts started")]
    migration_attempts_total: counter,
    #[describe("Migration attempts with terminal failure")]
    #[label(name = "phase", default = ["starting", "waiting_for_lock", "schema", "reconciling", "validating", "idle", "stopping"])]
    migration_failures_total: counter,
    #[describe("Day tables in the current reconciliation pass")]
    migration_leaves_total: gauge,
    #[describe("Day tables completed in the current reconciliation pass")]
    migration_leaves_completed: gauge,
    #[describe("Day indexes built by this process")]
    migration_leaves_built: counter,
    #[describe("Invalid unattached day indexes repaired by this process")]
    migration_leaves_repaired: counter,
    #[describe("Existing attached day indexes verified by this process")]
    migration_leaves_skipped: counter,
    #[describe("Unix seconds of last real migration progress")]
    migration_last_progress_timestamp_seconds: gauge,
    #[describe("Migration attempt duration in seconds")]
    migration_duration_seconds: histogram,
    #[describe("Owned database cancellation requests and verified outcomes")]
    #[label(name = "outcome", default = ["requested", "confirmed", "unconfirmed"])]
    migration_cancellation_total: counter,
}
