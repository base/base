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
    #[label(name = "status", default = ["success", "reverted", "error"])]
    nullify_tx_outcome_total: counter,

    #[describe("Latency in seconds for nullify transaction confirmation")]
    nullify_tx_latency_seconds: histogram,

    #[describe("Total number of challenge transactions submitted")]
    challenge_tx_submitted_total: counter,

    #[describe("Total number of challenge transaction outcomes")]
    #[label(name = "status", default = ["success", "reverted", "error"])]
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

    #[describe("Total number of invalid TEE proposals detected (Path 1)")]
    invalid_tee_proposal_detected_total: counter,

    #[describe("Total number of fraudulent ZK challenges detected (Path 2)")]
    fraudulent_zk_challenge_detected_total: counter,

    #[describe("Total number of invalid ZK proposals detected (Path 3)")]
    invalid_zk_proposal_detected_total: counter,

    #[describe("Total number of invalid dual proposals detected (Path 4)")]
    invalid_dual_proposal_detected_total: counter,

    #[describe("Total number of resolve transaction outcomes")]
    #[label(name = "status", default = ["success", "reverted", "error", "already_resolved"])]
    resolve_tx_outcome_total: counter,

    #[describe("Total number of claimCredit transactions submitted")]
    claim_credit_tx_submitted_total: counter,

    #[describe("Total number of claimCredit transaction outcomes")]
    #[label(name = "status", default = ["success", "reverted", "error"])]
    claim_credit_tx_outcome_total: counter,

    #[describe("Latency in seconds for bond transaction confirmation")]
    bond_tx_latency_seconds: histogram,

    #[describe("Number of games currently tracked for bond claiming")]
    bonds_tracked: gauge,

    #[describe("Total number of bonds successfully claimed")]
    bonds_completed_total: counter,

    #[describe("Total number of bonds dropped because recipient changed after resolve")]
    bonds_not_claimable_total: counter,

    #[describe("Total bond discovery scans performed")]
    #[label(name = "scan_type", default = ["full", "incremental"])]
    bond_discovery_scans_total: counter,

    #[describe("Total claimable games found by bond discovery")]
    bond_discovery_games_found_total: counter,

    #[describe("Total bond evaluation failures by error type")]
    #[label(name = "error_type", default = ["game_fetch", "bond_read", "phase_read"])]
    bond_evaluation_errors_total: counter,

    #[describe("Total anchor state update transaction outcomes")]
    #[label(name = "status", default = ["success", "error", "skipped"])]
    anchor_update_tx_outcome_total: counter,

    #[describe(
        "Number of otherwise-removable games currently retained while awaiting anchor state update"
    )]
    anchor_update_retained_games: gauge,

    #[describe(
        "Total games retained past bond lifecycle completion while awaiting anchor state update"
    )]
    anchor_update_retained_games_total: counter,

    #[describe(
        "L2 block number of the most recent anchor state successfully advanced by this challenger. \
         Monotonically increases as the challenger drives the anchor forward; absent until the \
         first successful setAnchorState() observation."
    )]
    #[no_zero]
    anchor_l2_block_number: gauge,

    #[describe("Challenger account balance in wei")]
    account_balance_wei: gauge,
}

impl ChallengerMetrics {
    /// Label value for a successfully confirmed transaction.
    pub const STATUS_SUCCESS: &str = "success";

    /// Label value for a reverted transaction.
    pub const STATUS_REVERTED: &str = "reverted";

    /// Label value for a transaction that failed to send.
    pub const STATUS_ERROR: &str = "error";

    /// Label value when a resolve was skipped because the game was already
    /// resolved on-chain (e.g. by another actor).
    pub const STATUS_ALREADY_RESOLVED: &str = "already_resolved";

    /// Label value when an anchor update was skipped (game not eligible).
    pub const STATUS_SKIPPED: &str = "skipped";

    /// Label value for a game fetch failure during bond evaluation.
    pub const EVAL_ERROR_GAME_FETCH: &str = "game_fetch";

    /// Label value for a bond recipient/zk prover read failure.
    pub const EVAL_ERROR_BOND_READ: &str = "bond_read";

    /// Label value for a bond phase determination failure.
    pub const EVAL_ERROR_PHASE_READ: &str = "phase_read";
}
