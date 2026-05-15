//! Challenger v2 metrics.
//!
//! Metrics are aggregated by category (no per-game labels) to keep
//! cardinality bounded. To identify which game a metric event refers to,
//! correlate with structured logs that always carry the same fields:
//!
//! - `game` (address)
//! - `action` (when applicable: `challenge`, `nullify_tee`, `nullify_zk`)

base_metrics::define_metrics! {
    challenger_v2,
    struct = ChallengerMetrics,

    #[describe("Total submit transaction outcomes, labeled by action and status.")]
    #[label(name = "action", default = ["challenge", "nullify_tee", "nullify_zk"])]
    #[label(name = "status", default = ["success", "reverted", "error"])]
    submit_outcome_total: counter,

    #[describe("Per-submit transaction duration in seconds, labeled by action.")]
    #[label(name = "action", default = ["challenge", "nullify_tee", "nullify_zk"])]
    submit_duration_seconds: histogram,

    #[describe("Total game-worker pipeline outcomes, labeled by terminal stage.")]
    #[label(name = "outcome", default = [
        "no_violation",
        "dispatched",
        "validation_error",
        "proof_error",
        "send_dropped",
    ])]
    game_worker_outcome_total: counter,
}

impl ChallengerMetrics {
    /// Label value for `DisputeAction::Challenge`.
    pub const ACTION_CHALLENGE: &str = "challenge";
    /// Label value for `DisputeAction::NullifyTee`.
    pub const ACTION_NULLIFY_TEE: &str = "nullify_tee";
    /// Label value for `DisputeAction::NullifyZk`.
    pub const ACTION_NULLIFY_ZK: &str = "nullify_zk";

    /// Label value for submit status `success` (tx mined and EVM status `true`).
    pub const SUBMIT_STATUS_SUCCESS: &str = "success";
    /// Label value for submit status `reverted` (tx mined but EVM status `false`).
    pub const SUBMIT_STATUS_REVERTED: &str = "reverted";
    /// Label value for submit status `error` (any pre-mining failure: contract
    /// revert at gas estimation, nonce conflict, RPC failure, signing error).
    pub const SUBMIT_STATUS_ERROR: &str = "error";

    /// Worker outcome: validator confirmed every intermediate root.
    pub const GAME_WORKER_OUTCOME_NO_VIOLATION: &str = "no_violation";
    /// Worker outcome: a dispute request reached the submission task.
    pub const GAME_WORKER_OUTCOME_DISPATCHED: &str = "dispatched";
    /// Worker outcome: validation failed (RPC, on-chain read, output check).
    pub const GAME_WORKER_OUTCOME_VALIDATION_ERROR: &str = "validation_error";
    /// Worker outcome: proof generation failed (ZK retries exhausted, etc.).
    pub const GAME_WORKER_OUTCOME_PROOF_ERROR: &str = "proof_error";
    /// Worker outcome: submission channel closed before the worker could send.
    pub const GAME_WORKER_OUTCOME_SEND_DROPPED: &str = "send_dropped";
}
