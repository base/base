//! Metrics for bundle metering.

base_metrics::define_metrics! {
    reth_metering
    #[describe("Count of pending trie cache hits")]
    pending_trie_cache_hits: counter,
    #[describe("Count of pending trie cache misses")]
    pending_trie_cache_misses: counter,
    #[describe("Time taken to compute pending trie on cache miss")]
    pending_trie_compute_duration: histogram,
    #[describe("Number of storage slots modified")]
    storage_slots_modified: histogram,
    #[describe("Number of accounts modified")]
    accounts_modified: histogram,
}
