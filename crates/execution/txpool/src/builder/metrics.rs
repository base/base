//! Metrics for the builder RPC handler.

base_metrics::define_metrics! {
    txpool.builder_rpc
    #[describe("Transactions successfully inserted into the pool")]
    txs_inserted: counter,
    #[describe("Transactions that failed to decode")]
    decode_errors: counter,
    #[describe("Transactions rejected by the pool")]
    #[label(name = "reason", default = [
        "already_imported",
        "replacement_underpriced",
        "fee_cap_below_minimum",
        "spammer_exceeded_capacity",
        "discarded_on_insert",
        "invalid_transaction",
        "conflicting_tx_type",
        "other",
    ])]
    txs_rejected: counter,
    #[describe("Time to insert a transaction in the local txpool")]
    insert_duration: histogram,
}
