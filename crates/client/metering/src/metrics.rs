//! Metrics for bundle metering.

use metrics::{Counter, Histogram};
use metrics_derive::Metrics;

/// Metrics for the `reth_metering` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
/// - Counters are monotonic event counts.
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_metering")]
pub(crate) struct Metrics {
    /// Count of pending trie cache hits.
    #[metric(describe = "Count of pending trie cache hits")]
    pub pending_trie_cache_hits: Counter,

    /// Count of pending trie cache misses (trie computation required).
    #[metric(describe = "Count of pending trie cache misses")]
    pub pending_trie_cache_misses: Counter,

    /// Time taken to compute pending trie (cache miss).
    #[metric(describe = "Time taken to compute pending trie on cache miss")]
    pub pending_trie_compute_duration: Histogram,

    /// Number of storage slots modified.
    #[metric(describe = "Number of storage slots modified")]
    pub storage_slots_modified: Histogram,
}
