//! Metrics for transaction pool readers.

base_metrics::define_metrics! {
    txpool.reader
    #[describe("Total reader loop iterations")]
    #[label(builder_url)]
    iterations: counter,
    #[describe("Total transactions read from the pool iterator")]
    #[label(builder_url)]
    txs_read: counter,
    #[describe("Total transactions queued after per-destination deduplication")]
    #[label(builder_url)]
    txs_sent: counter,
    #[describe("Total transactions skipped by the validator")]
    #[label(builder_url)]
    txs_ignored: counter,
    #[describe("Current number of entries in the destination dedup cache")]
    #[label(builder_url)]
    dedup_cache_size: gauge,
}

base_metrics::define_metrics! {
    txpool.reader,
    struct = SnapshotMetrics,
    #[describe("Time one destination reader's pool read (best_transactions) takes, including the pool read-lock wait")]
    snapshot_seconds: histogram,
    #[describe("Forwarding destinations, each of which reads the pool independently")]
    destinations: gauge,
}
