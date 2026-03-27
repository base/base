//! Proposer metrics.

base_metrics::define_metrics! {
    base_proposer

    #[describe("Proposer is running")]
    up: gauge,

    #[describe("Total number of L2 output proposals submitted")]
    l2_output_proposals_total: counter,

    #[describe("Proposer account balance in wei")]
    account_balance_wei: gauge,

    #[describe("Total number of TEE proofs skipped due to invalid signer")]
    tee_signer_invalid_total: counter,
}
