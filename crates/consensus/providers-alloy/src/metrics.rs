//! Metrics for the Alloy providers.

base_metrics::define_metrics! {
    base_providers
    #[describe("Number of cache hits in chain provider")]
    #[label(name = "cache", default = ["header_by_hash", "receipts_by_hash", "block_info_and_tx", "block_by_number"])]
    chain_cache_hits: counter,
    #[describe("Number of cache misses in chain provider")]
    #[label(name = "cache", default = ["header_by_hash", "receipts_by_hash", "block_info_and_tx", "block_by_number"])]
    chain_cache_misses: counter,
    #[describe("Number of RPC calls made by chain provider")]
    #[label(name = "method", default = ["header_by_hash", "receipts_by_hash", "block_by_hash", "block_number"])]
    chain_rpc_calls: counter,
    #[describe("Number of RPC errors in chain provider")]
    #[label(name = "method", default = ["header_by_hash", "receipts_by_hash", "block_by_hash", "block_number"])]
    chain_rpc_errors: counter,
    #[describe("Number of requests made to beacon client")]
    #[label(name = "method", default = ["spec", "genesis", "blobs"])]
    beacon_requests: counter,
    #[describe("Number of errors in beacon client requests")]
    #[label(name = "method", default = ["spec", "genesis", "blobs"])]
    beacon_errors: counter,
    #[describe("Number of requests made to L2 chain provider")]
    #[label(name = "method", default = ["l2_block_ref_by_label", "l2_block_ref_by_hash", "l2_block_ref_by_number"])]
    l2_chain_requests: counter,
    #[describe("Number of errors in L2 chain provider requests")]
    #[label(name = "method", default = ["l2_block_ref_by_label", "l2_block_ref_by_hash", "l2_block_ref_by_number"])]
    l2_chain_errors: counter,
    #[describe("Number of blob sidecar fetches")]
    blob_fetches: counter,
    #[describe("Number of blob sidecar fetch errors")]
    blob_fetch_errors: counter,
    #[describe("Duration of provider requests in seconds")]
    request_duration: histogram,
    #[describe("Number of active entries in provider caches")]
    #[label(name = "cache", default = ["header_by_hash", "receipts_by_hash", "block_info_and_tx"])]
    cache_entries: gauge,
    #[describe("Memory usage of provider caches in bytes")]
    #[label(name = "cache", default = ["header_by_hash", "receipts_by_hash", "block_info_and_tx"])]
    cache_memory_bytes: gauge,
}
