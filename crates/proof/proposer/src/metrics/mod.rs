/// Gauge: proposer build info, labelled with `version`.
pub const INFO: &str = "base_proposer_info";

/// Gauge: proposer is running (set to 1 at startup).
pub const UP: &str = "base_proposer_up";

/// Counter: total number of L2 output proposals submitted.
pub const L2_OUTPUT_PROPOSALS_TOTAL: &str = "base_proposer_l2_output_proposals_total";

/// Gauge: current depth of the proof queue.
pub const PROOF_QUEUE_DEPTH: &str = "base_proposer_proof_queue_depth";

/// Counter: total cache hits, labelled with `cache_name`.
pub const CACHE_HITS_TOTAL: &str = "base_proposer_cache_hits_total";

/// Counter: total cache misses, labelled with `cache_name`.
pub const CACHE_MISSES_TOTAL: &str = "base_proposer_cache_misses_total";

/// Gauge: proposer account balance in wei.
pub const ACCOUNT_BALANCE_WEI: &str = "base_proposer_account_balance_wei";

/// Label key for cache name.
pub const LABEL_CACHE_NAME: &str = "cache_name";

/// Label key for version.
pub const LABEL_VERSION: &str = "version";

/// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
pub fn record_startup_metrics(version: &str) {
    metrics::gauge!(INFO, LABEL_VERSION => version.to_string()).set(1.0);
    metrics::gauge!(UP).set(1.0);
}
