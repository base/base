//! Metrics for the `eth_sendBundle` RPC handler.

base_metrics::define_metrics! {
    txpool.bundle_rpc
    #[describe("Bundles successfully inserted into the pool")]
    txs_inserted: counter,
    #[describe("Bundles that failed to decode")]
    decode_errors: counter,
    #[describe("Bundles that failed signer recovery")]
    recovery_errors: counter,
    #[describe("Bundles rejected by request validation")]
    #[label(name = "reason", default = [
        "invalid_tx_count",
        "block_number_past",
        "block_number_too_far",
        "min_timestamp_too_far",
        "max_timestamp_past",
        "max_timestamp_too_far",
        "min_after_max_timestamp",
        "unsupported_field",
    ])]
    validation_errors: counter,
    #[describe("Bundles rejected by the pool")]
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
    #[describe("Requests rejected because eth_sendBundle is disabled")]
    not_enabled: counter,
}
