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
    #[describe("Canonical blocks recorded into the stale-storage table")]
    recorded_blocks_total: counter,
    #[describe("Seconds of canonical history the stale-storage table holds")]
    #[no_zero]
    table_age_seconds: gauge,
    #[describe("Canonical-state notifications the stale-storage recorder missed because its stream lagged")]
    canonical_stream_lagged_total: counter,
}
