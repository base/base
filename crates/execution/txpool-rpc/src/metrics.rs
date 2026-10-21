//! Metrics for `base_sendSwapTransaction` ingress.
//!
//! All labels are low-cardinality static categories from the RPC error vocabulary; addresses
//! and transaction hashes are never used as label values.

base_metrics::define_metrics! {
    txpool.swap_transaction,
    struct = SwapTransactionMetrics,
    #[describe("base_sendSwapTransaction rejections counted instead of journaled: requests that fail before validation or whose sender is not authorized for the requested capability")]
    #[label(
        name = "reason",
        default = [
            "principal_not_allowed",
            "request_too_large",
            "sibling_decode_failed",
            "envelope_invalid",
            "envelope_signature_invalid",
            "internal_error"
        ]
    )]
    unjournaled_rejections: counter,
}
