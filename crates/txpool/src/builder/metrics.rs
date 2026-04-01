//! Metrics for the builder RPC handler.

base_metrics::define_metrics! {
    txpool.builder_rpc
    #[describe("Transactions successfully inserted into the pool")]
    txs_inserted: counter,
    #[describe("Transactions that failed to decode")]
    decode_errors: counter,
    #[describe("Transactions rejected by the pool")]
    txs_rejected: counter,
    #[describe("Time to insert a transaction in the local txpool")]
    insert_duration: histogram,
}
