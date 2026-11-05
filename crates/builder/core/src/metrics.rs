//! Builder metrics for the metering store and resource-metering startup configuration.

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
}
