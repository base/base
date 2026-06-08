//! Proposer metrics.

base_metrics::define_metrics! {
    base_proposer
    #[describe("Proposer is running")]
    up: gauge,

    #[describe("Proposer account balance in wei")]
    #[no_zero]
    account_balance_wei: gauge,

    #[describe("Most recently proposed L2 block number")]
    #[no_zero]
    last_proposed_block: gauge,

    #[describe("Most recently collected (proved) L2 block number awaiting submission")]
    #[no_zero]
    last_collected_block: gauge,

    #[describe("Proof tasks currently in flight")]
    inflight_proofs: gauge,

    #[describe("Proved results awaiting sequential submission")]
    proved_queue_depth: gauge,

    #[describe("Total pending retries across all target blocks")]
    pipeline_retries: gauge,

    #[describe("Total proof dispatch outcomes from the prover service")]
    #[label(name = "outcome", default = ["accepted", "failed"])]
    proof_dispatch_total: counter,

    #[describe("Total proof collection outcomes returned by the proof collector")]
    #[label(name = "outcome", default = ["ready", "failed"])]
    proof_collection_total: counter,

    #[describe("Total proof statuses received when polling the prover service")]
    #[label(name = "status", default = ["queued", "running", "succeeded", "failed"])]
    proof_status_received_total: counter,

    #[describe("Total number of proof retries scheduled after a failed dispatch or collection")]
    proof_retries_total: counter,

    #[describe("Latest safe (or finalized) L2 block number")]
    #[no_zero]
    safe_head: gauge,

    #[describe("Total number of L2 output proposals submitted")]
    l2_output_proposals_total: counter,

    #[describe("Total number of TEE proofs skipped due to invalid signer")]
    tee_signer_invalid_total: counter,

    #[describe("Total errors by type")]
    #[label(
        name = "error_type",
        default = [
            "rpc",
            "prover",
            "contract",
            "tx_reverted",
            "config",
            "internal",
            "tx_manager",
            "game_already_exists"
        ]
    )]
    errors_total: counter,

    #[describe("Total output root mismatches detected at submit time")]
    root_mismatch_total: counter,

    #[describe("Time to generate a single proof (seconds)")]
    proof_duration_seconds: histogram,

    #[describe("Time for one pipeline tick (seconds)")]
    tick_duration_seconds: histogram,

    #[describe("Total time to validate and submit a proposal (seconds)")]
    proposal_total_duration_seconds: histogram,

    #[describe("Time for propose_output L1 transaction (seconds)")]
    proposal_l1_tx_duration_seconds: histogram,
}

impl Metrics {
    /// Label value for an accepted dispatch outcome.
    pub const DISPATCH_OUTCOME_ACCEPTED: &str = "accepted";

    /// Label value for a failed dispatch outcome.
    pub const DISPATCH_OUTCOME_FAILED: &str = "failed";

    /// Label value for a ready (successfully collected) proof.
    pub const COLLECTION_OUTCOME_READY: &str = "ready";

    /// Label value for a failed proof collection.
    pub const COLLECTION_OUTCOME_FAILED: &str = "failed";

    /// Label value for a queued proof status response.
    pub const PROOF_STATUS_QUEUED: &str = "queued";

    /// Label value for a running proof status response.
    pub const PROOF_STATUS_RUNNING: &str = "running";

    /// Label value for a succeeded proof status response.
    pub const PROOF_STATUS_SUCCEEDED: &str = "succeeded";

    /// Label value for a failed proof status response.
    pub const PROOF_STATUS_FAILED: &str = "failed";

    /// Records that the proposer service has started by setting the `up` gauge to 1.
    pub fn record_startup() {
        Self::up().set(1.0);
    }
}
