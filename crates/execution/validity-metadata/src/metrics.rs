//! Metrics for the validity metadata recorder.

base_metrics::define_metrics! {
    builder.validity_metadata,
    struct = ValidityMetadataMetrics,
    #[describe("Block records accepted into the validity metadata write queue")]
    records_queued: counter,
    #[describe("Block records lost before reaching durable storage, labeled by cause")]
    #[label(name = "reason", default = ["queue_full", "closed", "invalid", "write_failed"])]
    records_dropped: counter,
    #[describe("Block records durably committed, labeled by whether they were new or identical retries")]
    #[label(name = "outcome", default = ["inserted", "unchanged"])]
    records_written: counter,
    #[describe("Block records whose block hash has conflicting classifications and is answered with an error")]
    records_conflicting: counter,
}
