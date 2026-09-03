//! Metrics for the storage prefetch pool.

base_metrics::define_metrics! {
    storage.prefetch,
    struct = PrefetchMetrics,
    #[describe("Wall time in seconds of one prefetch storage read")]
    read_seconds: histogram,
    #[describe("Prefetch hint batches received from producers")]
    hints_total: counter,
    #[describe("Slot reads enqueued to prefetch workers")]
    slots_enqueued_total: counter,
    #[describe("Slot reads dropped because a worker queue was full")]
    slots_dropped_total: counter,
    #[describe("Hinted slot reads that failed, from a read error or an unavailable state provider")]
    read_errors_total: counter,
}
