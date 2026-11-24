//! Metrics for the log-only stale-storage meter.

base_metrics::define_metrics! {
    base_payload.stale_storage,
    struct = StaleStorageMetrics,
    #[describe("Transactions a stale-storage limit would have flagged, by scope and limit set, counted once per payload build attempt")]
    #[label(scope)]
    #[label(limits)]
    would_flag_total: counter,
    #[describe("Block-scope stale storage slots committed per payload build")]
    build_stale_slots: histogram,
    #[describe("Changed pre-existing storage slots recorded from canonical blocks, by gate and age")]
    #[label(gate)]
    #[label(age_bucket)]
    canonical_stale_slots_total: counter,
    #[describe("Canonical blocks recorded into the stale-storage cache")]
    recorded_blocks_total: counter,
    #[describe("Seconds of canonical history the stale-storage cache holds")]
    #[no_zero]
    cache_age_seconds: gauge,
    #[describe("Changed storage slots written into the stale-storage cache; each write first looks the slot up")]
    cache_writes_total: counter,
    #[describe("Stale-storage cache writes that evicted another slot still within the window, so it reads stale early; raise the cache size when this exceeds about 1% of cache_writes_total")]
    cache_unexpired_evictions_total: counter,
    #[describe("Share of stale-storage cache entries within the window, sampled from 64 sets per recorded canonical notification")]
    #[no_zero]
    cache_occupancy_ratio: gauge,
    #[describe("Changed storage slots the stale-storage recorder did not write, because their block predates the cache's base hour, is past its stamp range, or is timestamped before the last recorded hour")]
    cache_dropped_writes_total: counter,
    #[describe("Stale-storage cache entries emptied because the blocks that wrote them were reorged out")]
    cache_reorg_invalidations_total: counter,
    #[describe("Canonical-state notifications the stale-storage recorder missed because its stream lagged")]
    canonical_stream_lagged_total: counter,
}
