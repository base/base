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
    #[describe("Transaction observability BRIN block ranges summarized")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_brin_ranges_summarized: counter,
    #[describe("Transaction observability day partitions whose BRIN summary was skipped after lock_timeout")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_brin_summary_lock_timeouts: counter,
    #[describe("Transaction observability BRIN summary pass failures")]
    transaction_event_brin_summary_failures: counter,
}
