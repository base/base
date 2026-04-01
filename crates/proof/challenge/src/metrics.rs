//! Challenger metrics constants.

base_metrics::define_metrics! {
    base_challenger,
    struct = ChallengerMetrics,

    #[describe("Challenger is running")]
    up: gauge,

    #[describe("Total number of games evaluated during scanning")]
    games_scanned_total: counter,

    #[describe("Latest factory index scanned by the game scanner")]
    scan_head: gauge,

    #[describe("Total number of games found to be invalid during validation")]
    games_invalid_total: counter,

    #[describe("Total number of validation errors")]
    validation_errors_total: counter,

    #[describe("Latency in seconds for output root validation")]
    validation_latency_seconds: histogram,

    #[describe("Total number of nullify transactions submitted")]
    nullify_tx_submitted_total: counter,

    #[describe("Total number of nullify transaction outcomes")]
    #[label(status)]
    nullify_tx_outcome_total: counter,

    #[describe("Latency in seconds for nullify transaction confirmation")]
    nullify_tx_latency_seconds: histogram,

    #[describe("Total number of challenge transactions submitted")]
    challenge_tx_submitted_total: counter,

    #[describe("Total number of challenge transaction outcomes")]
    #[label(status)]
    challenge_tx_outcome_total: counter,

    #[describe("Latency in seconds for challenge transaction confirmation")]
    challenge_tx_latency_seconds: histogram,

    #[describe("Total number of proof retries after failure")]
    proof_retries_total: counter,

    #[describe("Number of in-flight proof sessions")]
    pending_proofs: gauge,

    #[describe("Total number of TEE proof attempts")]
    tee_proof_attempts_total: counter,

    #[describe("Total number of TEE proofs successfully obtained")]
    tee_proof_obtained_total: counter,

    #[describe("Total number of TEE proof failures that fell back to ZK")]
    tee_proof_fallback_total: counter,

    #[describe("Total number of fraudulent ZK challenges detected (Path 2)")]
    fraudulent_zk_challenge_detected_total: counter,

    #[describe("Total number of invalid ZK proposals detected (Path 3)")]
    invalid_zk_proposal_detected_total: counter,
}

impl ChallengerMetrics {
    /// Label value for a successfully confirmed transaction.
    pub const STATUS_SUCCESS: &str = "success";

    /// Label value for a reverted transaction.
    pub const STATUS_REVERTED: &str = "reverted";

    /// Label value for a transaction that failed to send.
    pub const STATUS_ERROR: &str = "error";
}
