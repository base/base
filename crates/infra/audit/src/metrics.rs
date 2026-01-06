use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;

/// Metrics for audit operations including Kafka reads, S3 writes, and event processing.
#[derive(Metrics, Clone)]
#[metrics(scope = "tips_audit")]
pub struct Metrics {
    /// Duration of archive_event operations.
    #[metric(describe = "Duration of archive_event")]
    pub archive_event_duration: Histogram,

    /// Age of event when processed (now - event timestamp).
    #[metric(describe = "Age of event when processed (now - event timestamp)")]
    pub event_age: Histogram,

    /// Duration of Kafka read_event operations.
    #[metric(describe = "Duration of Kafka read_event")]
    pub kafka_read_duration: Histogram,

    /// Duration of Kafka commit operations.
    #[metric(describe = "Duration of Kafka commit")]
    pub kafka_commit_duration: Histogram,

    /// Duration of update_bundle_history operations.
    #[metric(describe = "Duration of update_bundle_history")]
    pub update_bundle_history_duration: Histogram,

    /// Duration of updating all transaction indexes.
    #[metric(describe = "Duration of update all transaction indexes")]
    pub update_tx_indexes_duration: Histogram,

    /// Duration of S3 get_object operations.
    #[metric(describe = "Duration of S3 get_object")]
    pub s3_get_duration: Histogram,

    /// Duration of S3 put_object operations.
    #[metric(describe = "Duration of S3 put_object")]
    pub s3_put_duration: Histogram,

    /// Total events processed.
    #[metric(describe = "Total events processed")]
    pub events_processed: Counter,

    /// Total S3 writes skipped due to deduplication.
    #[metric(describe = "Total S3 writes skipped due to dedup")]
    pub s3_writes_skipped: Counter,

    /// Number of in-flight archive tasks.
    #[metric(describe = "Number of in-flight archive tasks")]
    pub in_flight_archive_tasks: Gauge,
}
