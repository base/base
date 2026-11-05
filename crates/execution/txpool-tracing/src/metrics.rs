//! Metrics for transaction tracing.

base_metrics::define_metrics! {
    reth_transaction_tracing
    #[describe("Time taken for a transaction to be included in a block from when it's marked as pending")]
    #[label(name = "flow", default = ["standard", "validity"])]
    inclusion_duration: histogram,
    #[describe("Time taken for a transaction to be included in a flashblock from when it's marked as pending")]
    #[label(name = "flow", default = ["standard", "validity"])]
    fb_inclusion_duration: histogram,
    #[describe("Number of transactions included in a flashblock within the healthy threshold")]
    #[label(name = "flow", default = ["standard", "validity"])]
    fb_healthy_inclusions: counter,
    #[describe("Number of transactions that exceeded the healthy flashblock inclusion threshold")]
    #[label(name = "flow", default = ["standard", "validity"])]
    fb_slow_inclusions: counter,
    #[describe("Number of transactions included in a block within the healthy threshold")]
    #[label(name = "flow", default = ["standard", "validity"])]
    healthy_inclusions: counter,
    #[describe("Number of transactions that exceeded the healthy block inclusion threshold")]
    #[label(name = "flow", default = ["standard", "validity"])]
    slow_inclusions: counter,
    #[describe("End-to-end time from first submission to block inclusion for a (sender, nonce) pair")]
    #[label(name = "flow", default = ["standard", "validity"])]
    e2e_inclusion_duration: histogram,
    #[describe("Number of replacement transactions per (sender, nonce) pair")]
    replacement_count: histogram,
    #[describe("Total number of nonce-slot replacements observed")]
    nonce_replacements: counter,
}
