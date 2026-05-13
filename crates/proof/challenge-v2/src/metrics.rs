//! Challenger v2 metrics.
//!
//! Metrics are aggregated by category (no per-game labels) to keep
//! cardinality bounded. To identify which game a metric event refers to,
//! correlate with structured logs that always carry the same fields:
//!
//! - `game` (address)
//! - `factory_index`
//! - `situation` (when applicable: `TeeWrong`, `ZkWrong`, `FraudulentZkChallenge`)
//! - `action` (when applicable: `challenge`, `nullify_tee`, `nullify_zk`)
//! - `kind` (when applicable: `tee`, `zk`)
//! - `step` (bond worker: `resolve`, `unlock`, `withdraw`, `close_game`)

base_metrics::define_metrics! {
    challenger_v2,
    struct = ChallengerMetrics,

    #[describe("Challenger v2 is running (1 = running).")]
    up: gauge,

    #[describe("Challenger sender account balance in wei.")]
    #[no_zero]
    account_balance_wei: gauge,

    #[describe("Unix timestamp (seconds) of the last successful scanner tick.")]
    #[no_zero]
    last_scan_at: gauge,

    #[describe("Total scanner tick attempts.")]
    scan_attempts_total: counter,

    #[describe("Total scanner tick failures (RPC errors etc.).")]
    scan_errors_total: counter,

    #[describe("Per-tick scanner duration in seconds.")]
    scan_duration_seconds: histogram,

    #[describe("Total games discovered as actionable across the lifetime.")]
    games_discovered_total: counter,

    #[describe("Number of IN_PROGRESS games currently tracked by the scanner.")]
    in_progress_games: gauge,

    #[describe("Latest known anchor L2 block number.")]
    #[no_zero]
    anchor_l2_block: gauge,

    #[describe("Number of GameWorker tasks currently running in the pool.")]
    workers_active: gauge,

    #[describe("Total GameWorker tasks spawned across the lifetime.")]
    workers_spawned_total: counter,

    #[describe("Total GameWorker tasks that completed normally.")]
    workers_completed_total: counter,

    #[describe("Total GameWorker tasks that ended in a panic or error.")]
    workers_failed_total: counter,

    #[describe("Per-game validation duration in seconds (compute output roots + compare).")]
    validation_duration_seconds: histogram,

    #[describe("Total validation results, labeled by outcome.")]
    #[label(name = "outcome", default = ["no_violation", "violation", "error"])]
    validations_total: counter,

    #[describe("Total violations detected, labeled by `ViolationSituation`.")]
    #[label(
        name = "situation",
        default = ["TeeWrong", "ZkWrong", "FraudulentZkChallenge"]
    )]
    violations_detected_total: counter,

    #[describe("Total proof generation attempts, labeled by proof kind.")]
    #[label(name = "kind", default = ["tee", "zk"])]
    proof_attempts_total: counter,

    #[describe("Total proof generation outcomes, labeled by kind and status.")]
    #[label(name = "kind", default = ["tee", "zk"])]
    #[label(name = "status", default = ["success", "failed", "timeout"])]
    proof_outcome_total: counter,

    #[describe("Per-proof duration in seconds, labeled by kind.")]
    #[label(name = "kind", default = ["tee", "zk"])]
    proof_duration_seconds: histogram,

    #[describe(
        "Total times a TEE proof produced a wrong root and we fell back to a ZK challenge."
    )]
    tee_fallback_to_zk_total: counter,

    #[describe(
        "Total times prover could not produce a valid proof for an actionable violation."
    )]
    prover_stuck_total: counter,

    #[describe("Total submit transaction outcomes, labeled by action and status.")]
    #[label(name = "action", default = ["challenge", "nullify_tee", "nullify_zk"])]
    #[label(name = "status", default = ["success", "reverted", "error", "skipped_reverify"])]
    submit_outcome_total: counter,

    #[describe("Per-submit transaction duration in seconds, labeled by action.")]
    #[label(name = "action", default = ["challenge", "nullify_tee", "nullify_zk"])]
    submit_duration_seconds: histogram,

    #[describe("Number of BondWorker tasks currently running in the pool.")]
    bond_workers_active: gauge,

    #[describe("Total BondWorker tasks spawned across the lifetime.")]
    bond_workers_spawned_total: counter,

    #[describe("Total BondWorker tasks that completed the full bond lifecycle.")]
    bond_workers_completed_total: counter,

    #[describe("Total bond step outcomes, labeled by step and status.")]
    #[label(name = "step", default = ["resolve", "unlock", "withdraw", "close_game"])]
    #[label(name = "status", default = ["success", "reverted", "error", "skipped"])]
    bond_step_outcome_total: counter,

    #[describe("Per-bond-step transaction duration in seconds, labeled by step.")]
    #[label(name = "step", default = ["resolve", "unlock", "withdraw", "close_game"])]
    bond_step_duration_seconds: histogram,
}

impl ChallengerMetrics {
    /// Label value for `validations_total` indicating no violation was detected.
    pub const VALIDATION_OUTCOME_NO_VIOLATION: &str = "no_violation";
    /// Label value for `validations_total` indicating a violation was detected.
    pub const VALIDATION_OUTCOME_VIOLATION: &str = "violation";
    /// Label value for `validations_total` indicating an error occurred during validation.
    pub const VALIDATION_OUTCOME_ERROR: &str = "error";

    /// Label value for `ViolationSituation::TeeWrong`.
    pub const SITUATION_TEE_WRONG: &str = "TeeWrong";
    /// Label value for `ViolationSituation::ZkWrong`.
    pub const SITUATION_ZK_WRONG: &str = "ZkWrong";
    /// Label value for `ViolationSituation::FraudulentZkChallenge`.
    pub const SITUATION_FRAUDULENT_ZK_CHALLENGE: &str = "FraudulentZkChallenge";

    /// Label value for proof kind TEE.
    pub const KIND_TEE: &str = "tee";
    /// Label value for proof kind ZK.
    pub const KIND_ZK: &str = "zk";

    /// Label value for proof status `success`.
    pub const PROOF_STATUS_SUCCESS: &str = "success";
    /// Label value for proof status `failed` (non-timeout error).
    pub const PROOF_STATUS_FAILED: &str = "failed";
    /// Label value for proof status `timeout` (deadline exceeded).
    pub const PROOF_STATUS_TIMEOUT: &str = "timeout";

    /// Label value for `DisputeAction::Challenge`.
    pub const ACTION_CHALLENGE: &str = "challenge";
    /// Label value for `DisputeAction::NullifyTee`.
    pub const ACTION_NULLIFY_TEE: &str = "nullify_tee";
    /// Label value for `DisputeAction::NullifyZk`.
    pub const ACTION_NULLIFY_ZK: &str = "nullify_zk";

    /// Label value for submit status `success`.
    pub const SUBMIT_STATUS_SUCCESS: &str = "success";
    /// Label value for submit status `reverted` (tx confirmed but reverted on-chain).
    pub const SUBMIT_STATUS_REVERTED: &str = "reverted";
    /// Label value for submit status `error` (tx failed to send or other RPC error).
    pub const SUBMIT_STATUS_ERROR: &str = "error";
    /// Label value for submit status `skipped_reverify` (live state changed before submission).
    pub const SUBMIT_STATUS_SKIPPED_REVERIFY: &str = "skipped_reverify";

    /// Label value for bond step `resolve`.
    pub const BOND_STEP_RESOLVE: &str = "resolve";
    /// Label value for bond step `unlock`.
    pub const BOND_STEP_UNLOCK: &str = "unlock";
    /// Label value for bond step `withdraw`.
    pub const BOND_STEP_WITHDRAW: &str = "withdraw";
    /// Label value for bond step `close_game`.
    pub const BOND_STEP_CLOSE_GAME: &str = "close_game";

    /// Label value for bond step status `success`.
    pub const BOND_STATUS_SUCCESS: &str = "success";
    /// Label value for bond step status `reverted`.
    pub const BOND_STATUS_REVERTED: &str = "reverted";
    /// Label value for bond step status `error`.
    pub const BOND_STATUS_ERROR: &str = "error";
    /// Label value for bond step status `skipped` (already in target state).
    pub const BOND_STATUS_SKIPPED: &str = "skipped";
}
