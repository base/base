//! Metrics for the state prefetch pool.

base_metrics::define_metrics! {
    state.prefetch,
    struct = PrefetchMetrics,
    #[describe("Wall time in seconds of one prefetch read")]
    #[label(name = "kind", default = ["slot", "account", "code"])]
    read_seconds: histogram,
    #[describe("Prefetch hint batches received from producers")]
    hints_total: counter,
    #[describe("Prefetch requests enqueued to workers")]
    requests_enqueued_total: counter,
    #[describe("Prefetch requests dropped because a worker queue was full")]
    requests_dropped_total: counter,
    #[describe("Prefetch reads that failed, from a read error or an unavailable state provider")]
    read_errors_total: counter,
}
