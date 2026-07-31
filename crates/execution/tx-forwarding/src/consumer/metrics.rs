//! Metrics for transaction consumers.

base_metrics::define_metrics! {
    txpool.consumer
    #[describe("Total consumer loop iterations")]
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
