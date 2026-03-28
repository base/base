//! Challenger metrics constants.

base_metrics::define_metrics_struct! {
    ChallengerMetrics, base_challenger,

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
}

impl ChallengerMetrics {
    /// Label value for a successfully confirmed transaction.
    pub const STATUS_SUCCESS: &str = "success";

    /// Label value for a reverted transaction.
    pub const STATUS_REVERTED: &str = "reverted";

    /// Label value for a transaction that failed to send.
    pub const STATUS_ERROR: &str = "error";
}
