//! Exporter and reader metrics.

base_metrics::define_metrics! {
    base_exoware_archive,
    struct = ArchiveMetrics,
    #[describe("Segments published by this node")]
    segments_published: counter,
    #[describe("Latency of one acknowledged store put in seconds")]
    put_latency: histogram,
    #[describe("Store puts retried after an ambiguous failure")]
    put_retries: counter,
    #[describe("Highest published segment observed by this node")]
    #[no_zero]
    highest_marker: gauge,
    #[describe("Blocks between the node's finalized block and the highest archived block")]
    #[no_zero]
    lag_blocks: gauge,
    #[describe("Archive answers rejected by verification")]
    #[label(name = "family", default = ["block_hash", "tx_hash", "state_history", "log_address", "log_topic", "header", "body", "receipts", "senders", "account_changes", "storage_changes", "marker", "config"])]
    faults: counter,
    #[describe("Set to 1 when export halted because a stored marker differs from the local rows")]
    quarantined: gauge,
    #[describe("Set to 1 when the reader disabled the archive link (config identity mismatch)")]
    link_disabled: gauge,
    #[describe("Hash lookups answered with the node's own null because the store failed before the hash resolved")]
    hash_fallbacks: counter,
    #[describe("Set to 1 while the exporter runs; 0 once it exited (shutdown or a terminal error)")]
    exporter_running: gauge,
    #[describe("Prune passes completed")]
    prune_runs: counter,
}
