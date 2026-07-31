//! Metrics for audit operations including event reads, S3 writes, and event processing.

base_metrics::define_metrics! {
    tips_audit
    #[describe("Duration of archive_event")]
    archive_event_duration: histogram,
    #[describe("Age of event when processed (now - event timestamp)")]
    event_age: histogram,
    #[describe("Duration of read_event")]
    read_duration: histogram,
    #[describe("Duration of event commit")]
    commit_duration: histogram,
    #[describe("Duration of update_bundle_history")]
    update_bundle_history_duration: histogram,
    #[describe("Duration of update all transaction indexes")]
    update_tx_indexes_duration: histogram,
    #[describe("Duration of S3 get_object")]
    s3_get_duration: histogram,
    #[describe("Duration of S3 put_object")]
    s3_put_duration: histogram,
    #[describe("Total events processed")]
    events_processed: counter,
    #[describe("Total S3 writes skipped due to dedup")]
    s3_writes_skipped: counter,
    #[describe("Total S3 conditional write conflicts (412/409)")]
    s3_conditional_conflicts: counter,
    #[describe("Number of in-flight archive tasks")]
    in_flight_archive_tasks: gauge,
    #[describe("Number of failed archive tasks")]
    failed_archive_tasks: counter,
    #[describe("Bundle event batches that failed to publish over RPC and were dropped")]
    #[label(name = "reason", default = ["rpc_error"])]
    rpc_publish_failures: counter,
    #[describe("Bundle events deduplicated by the RPC reader cache")]
    rpc_cache_hits: counter,
    #[describe("Bundle events that missed the RPC reader cache and were forwarded")]
    rpc_cache_misses: counter,
    #[describe("Bundle events dropped because the RPC reader channel could not accept them")]
    #[label(name = "kind", default = ["full", "closed"])]
    rpc_channel_send_failures: counter,
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
    #[describe("Transaction observability HTTP ingest batch size")]
    transaction_event_batch_size: histogram,
    #[describe("Duration of transaction observability Postgres batch writes")]
    transaction_event_batch_write_duration: histogram,
    #[describe("Transaction observability daily partitions created")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partitions_created: counter,
    #[describe("Expired transaction observability daily partitions dropped")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partitions_dropped: counter,
    #[describe("Age in seconds of the oldest retained transaction observability partition")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_oldest_partition_age_seconds: gauge,
    #[describe("UTC days from today to the newest premade transaction observability partition; only the advisory-lock winner refreshes this, so alert on max by retention_class (non-winners leave 0)")]
    #[label(name = "retention_class", default = ["hot", "warm", "cold"])]
    transaction_event_partitions_ahead_days: gauge,
    #[describe("Seconds since this process last completed a locked transaction observability partition maintenance pass; pods that never won stay at 0, so do not alert on plain min/max across replicas")]
    transaction_event_partition_maintenance_last_success_age_seconds: gauge,
    #[describe("Transaction observability partition maintenance failures")]
    transaction_event_partition_maintenance_failures: counter,
}
