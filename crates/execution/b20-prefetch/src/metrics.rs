//! Metrics for the B20 storage prefetch pool.

base_metrics::define_metrics! {
    b20.prefetch,
    struct = PrefetchMetrics,
    #[describe("Wall time in seconds of one prefetch storage read, including state-provider setup")]
    read_seconds: histogram,
    #[describe("Prefetch hint batches received from precompile dispatch")]
    hints_total: counter,
    #[describe("Slot reads enqueued to prefetch workers")]
    slots_enqueued_total: counter,
    #[describe("Slot reads dropped because a worker queue was full")]
    slots_dropped_total: counter,
    #[describe("Prefetch reads that failed with a state-provider error")]
    read_errors_total: counter,
}
