//! Metrics for transaction tracing.

base_metrics::define_metrics! {
    reth_transaction_tracing
    #[describe("Time taken for a transaction to be included in a block from when it's marked as pending")]
    inclusion_duration: histogram,
    #[describe("Time taken for a transaction to be included in a flashblock from when it's marked as pending")]
    fb_inclusion_duration: histogram,
    #[describe("Number of transactions included in a flashblock within the healthy threshold")]
    fb_healthy_inclusions: counter,
    #[describe("Number of transactions that exceeded the healthy flashblock inclusion threshold")]
    fb_slow_inclusions: counter,
    #[describe("Number of transactions included in a block within the healthy threshold")]
    healthy_inclusions: counter,
    #[describe("Number of transactions that exceeded the healthy block inclusion threshold")]
    slow_inclusions: counter,
    #[describe("End-to-end time from first submission to block inclusion for a (sender, nonce) pair")]
    e2e_inclusion_duration: histogram,
    #[describe("Number of replacement transactions per (sender, nonce) pair")]
    replacement_count: histogram,
    #[describe("Total number of nonce-slot replacements observed")]
    nonce_replacements: counter,
}
