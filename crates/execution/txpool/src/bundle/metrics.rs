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
    #[label(reason)]
    validation_errors: counter,
    #[describe("Bundles rejected by the pool")]
    #[label(reason)]
    txs_rejected: counter,
    #[describe("Requests rejected because eth_sendBundle is disabled")]
    not_enabled: counter,
}
