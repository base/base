//! Builder metrics for the shared metering store and startup configuration checks.

base_metrics::define_metrics! {
    base_builder,
    struct = BuilderMetrics,
    #[describe("Number of LRU evictions from MeteringStore")]
    metering_store_lru_evictions: counter,
    #[describe("Number of entries in the metering store that expired due to TTL")]
    metering_store_ttl_expirations: counter,
    #[describe("Size of MeteringStore")]
    metering_store_size: gauge,
    #[describe(
        "Builder started with resource metering enabled but no resource-metering schedule configured"
    )]
    resource_metering_schedule_missing_total: counter,
    #[describe("Metering responses that arrived after unmetered payload inclusion")]
    metering_late_arrival_total: counter,
    #[describe("Time between unmetered payload inclusion and metering data arrival (milliseconds)")]
    metering_late_arrival_latency_ms: histogram,
    #[describe("Execution time from late-arriving metering data (microseconds)")]
    metering_late_arrival_execution_time_us: histogram,
}
