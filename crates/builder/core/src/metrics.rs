//! Builder RPC metrics.

base_metrics::define_metrics! {
    base_builder,
    struct = BuilderMetrics,
    #[describe("Shadow validity injection decisions")]
    #[label(outcome)]
    shadow_validity_injection_total: counter,
    #[describe("Time between unmetered payload inclusion and metering data arrival (milliseconds)")]
    metering_late_arrival_latency_ms: histogram,
    #[describe("Metering responses that arrived after unmetered payload inclusion")]
    metering_late_arrival_total: counter,
    #[describe("Number of LRU evictions from MeteringStore")]
    metering_store_lru_evictions: counter,
    #[describe("Size of MeteringStore")]
    metering_store_size: gauge,
    #[describe("Number of entries in the metering store that expired due to TTL")]
    metering_store_ttl_expirations: counter,
    #[describe("Execution time from late-arriving metering data (microseconds)")]
    metering_late_arrival_execution_time_us: histogram,
}
