//! Metrics for audit operations including Kafka reads, S3 writes, and event processing.

base_metrics::define_metrics! {
    tips_audit
    #[describe("Duration of archive_event")]
    archive_event_duration: histogram,
    #[describe("Age of event when processed (now - event timestamp)")]
    event_age: histogram,
    #[describe("Duration of Kafka read_event")]
    kafka_read_duration: histogram,
    #[describe("Duration of Kafka commit")]
    kafka_commit_duration: histogram,
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
    #[describe("Number of in-flight archive tasks")]
    in_flight_archive_tasks: gauge,
    #[describe("Number of failed archive tasks")]
    failed_archive_tasks: counter,
}
