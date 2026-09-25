//! Metrics for the payload witness cache.

base_metrics::define_metrics! {
    base_witness_cache

    #[describe("Payload witness ingest attempts")]
    #[label(name = "outcome", default = ["cached", "retry", "skipped"])]
    ingest_attempts_total: counter,

    #[describe("Latency in seconds for debug_executePayload calls made while following the tip")]
    execute_payload_duration_seconds: histogram,

    #[describe("L2 blocks the follower has not cached yet")]
    #[no_zero]
    tip_lag_blocks: gauge,

    #[describe("Payload witnesses retained in memory")]
    #[no_zero]
    cached_blocks: gauge,

    #[describe("Payload witness lookups served by this process")]
    #[label(name = "outcome", default = ["hit", "miss", "invalid"])]
    lookups_total: counter,
}

impl Metrics {
    /// Ingest stored a witness.
    pub const INGEST_CACHED: &str = "cached";

    /// Ingest will retry this block.
    pub const INGEST_RETRY: &str = "retry";

    /// Ingest skipped a block that cannot be cached.
    pub const INGEST_SKIPPED: &str = "skipped";

    /// Lookup found a witness.
    pub const LOOKUP_HIT: &str = "hit";

    /// Lookup found no witness.
    pub const LOOKUP_MISS: &str = "miss";

    /// Lookup rejected the request.
    pub const LOOKUP_INVALID: &str = "invalid";
}
