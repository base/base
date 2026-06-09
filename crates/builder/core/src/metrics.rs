//! Builder metrics retained for metering-store and builder RPC support.

base_metrics::define_metrics! {
    reth_base_builder, struct = BuilderMetrics,
    #[describe("Number of metering store entries currently retained")]
    metering_store_size: gauge,
    #[describe("Number of metering store entries evicted by the LRU policy")]
    metering_store_lru_evictions: counter,
    #[describe("Number of metering store entries expired by TTL")]
    metering_store_ttl_expirations: counter,
    #[describe("Number of metering records that arrived after transaction inclusion")]
    metering_late_arrival_total: counter,
    #[describe("Milliseconds between unmetered inclusion and late metering arrival")]
    metering_late_arrival_latency_ms: histogram,
    #[describe("Late-arriving transaction execution time in microseconds")]
    metering_late_arrival_execution_time_us: histogram,
    #[describe("Late-arriving state-root calculation time in microseconds")]
    metering_late_arrival_state_root_time_us: histogram,
}
