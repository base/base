//! Metrics for the transaction consumer.

base_metrics::define_metrics! {
    txpool.consumer
    #[describe("Total consumer loop iterations")]
    iterations: counter,
    #[describe("Total transactions read from the pool iterator")]
    txs_read: counter,
    #[describe("Total transactions broadcast after deduplication")]
    txs_sent: counter,
    #[describe("Total transactions skipped by the validator")]
    txs_ignored: counter,
    #[describe("Current number of entries in the dedup cache")]
    dedup_cache_size: gauge,
}
