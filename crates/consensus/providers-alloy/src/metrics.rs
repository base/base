//! Metrics for the Alloy providers.

base_metrics::define_metrics! {
    base_providers
    #[describe("Number of cache hits in chain provider")]
    #[label(cache)]
    chain_cache_hits: counter,
    #[describe("Number of cache misses in chain provider")]
    #[label(cache)]
    chain_cache_misses: counter,
    #[describe("Number of RPC calls made by chain provider")]
    #[label(method)]
    chain_rpc_calls: counter,
    #[describe("Number of RPC errors in chain provider")]
    #[label(method)]
    chain_rpc_errors: counter,
    #[describe("Number of requests made to beacon client")]
    #[label(method)]
    beacon_requests: counter,
    #[describe("Number of errors in beacon client requests")]
    #[label(method)]
    beacon_errors: counter,
    #[describe("Number of requests made to L2 chain provider")]
    #[label(method)]
    l2_chain_requests: counter,
    #[describe("Number of errors in L2 chain provider requests")]
    #[label(method)]
    l2_chain_errors: counter,
    #[describe("Number of blob sidecar fetches")]
    blob_fetches: counter,
    #[describe("Number of blob sidecar fetch errors")]
    blob_fetch_errors: counter,
    #[describe("Duration of provider requests in seconds")]
    request_duration: histogram,
    #[describe("Number of active entries in provider caches")]
    #[label(cache)]
    cache_entries: gauge,
    #[describe("Memory usage of provider caches in bytes")]
    #[label(cache)]
    cache_memory_bytes: gauge,
}

impl Metrics {
    /// Initializes metrics for the Alloy providers.
    ///
    /// This does two things:
    /// * Describes various metrics.
    /// * Initializes metrics to 0 so they can be queried immediately.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        Self::chain_cache_hits("header_by_hash").absolute(0);
        Self::chain_cache_hits("receipts_by_hash").absolute(0);
        Self::chain_cache_hits("block_info_and_tx").absolute(0);
        Self::chain_cache_hits("block_by_number").absolute(0);

        Self::chain_cache_misses("header_by_hash").absolute(0);
        Self::chain_cache_misses("receipts_by_hash").absolute(0);
        Self::chain_cache_misses("block_info_and_tx").absolute(0);
        Self::chain_cache_misses("block_by_number").absolute(0);

        Self::chain_rpc_calls("header_by_hash").absolute(0);
        Self::chain_rpc_calls("receipts_by_hash").absolute(0);
        Self::chain_rpc_calls("block_by_hash").absolute(0);
        Self::chain_rpc_calls("block_number").absolute(0);

        Self::chain_rpc_errors("header_by_hash").absolute(0);
        Self::chain_rpc_errors("receipts_by_hash").absolute(0);
        Self::chain_rpc_errors("block_by_hash").absolute(0);
        Self::chain_rpc_errors("block_number").absolute(0);

        Self::beacon_requests("spec").absolute(0);
        Self::beacon_requests("genesis").absolute(0);
        Self::beacon_requests("blobs").absolute(0);

        Self::beacon_errors("spec").absolute(0);
        Self::beacon_errors("genesis").absolute(0);
        Self::beacon_errors("blobs").absolute(0);

        Self::l2_chain_requests("l2_block_ref_by_label").absolute(0);
        Self::l2_chain_requests("l2_block_ref_by_hash").absolute(0);
        Self::l2_chain_requests("l2_block_ref_by_number").absolute(0);

        Self::l2_chain_errors("l2_block_ref_by_label").absolute(0);
        Self::l2_chain_errors("l2_block_ref_by_hash").absolute(0);
        Self::l2_chain_errors("l2_block_ref_by_number").absolute(0);

        Self::blob_fetches().absolute(0);
        Self::blob_fetch_errors().absolute(0);

        Self::cache_entries("header_by_hash").set(0.0);
        Self::cache_entries("receipts_by_hash").set(0.0);
        Self::cache_entries("block_info_and_tx").set(0.0);

        Self::cache_memory_bytes("header_by_hash").set(0.0);
        Self::cache_memory_bytes("receipts_by_hash").set(0.0);
        Self::cache_memory_bytes("block_info_and_tx").set(0.0);
    }
}
