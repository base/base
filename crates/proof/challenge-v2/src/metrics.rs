//! Prometheus metrics emitted by the challenger.

base_metrics::define_metrics! {
    base_challenger_v2

    #[describe("Challenger service is running")]
    up: gauge,

    #[describe("Challenger L1 sender balance in wei")]
    #[no_zero]
    account_balance_wei: gauge,

    #[describe("Total game discovery scan ticks completed")]
    game_scan_ticks_total: counter,

    #[describe("Total game discovery scan errors")]
    game_scan_errors_total: counter,

    #[describe("Games with status IN_PROGRESS observed at the last scan tick")]
    games_in_progress: gauge,

    #[describe("Games returned by the last scan tick for worker processing")]
    games_to_process: gauge,

    #[describe("Game workers currently running")]
    game_workers_in_flight: gauge,

    #[describe("Game worker total lifetime in seconds")]
    game_worker_duration_seconds: histogram,

    #[describe("Proof generations currently in flight")]
    #[label(name = "kind", default = ["tee", "zk"])]
    proofs_in_flight: gauge,

    #[describe("Total proof generation outcomes")]
    #[label(name = "kind", default = ["tee", "zk"])]
    #[label(name = "status", default = ["ok", "fail"])]
    proofs_total: counter,

    #[describe("Proof generation duration in seconds")]
    #[label(name = "kind", default = ["tee", "zk"])]
    proof_duration_seconds: histogram,

    #[describe("Total bond discovery scan ticks completed")]
    bond_scan_ticks_total: counter,

    #[describe("Total bond discovery scan errors")]
    bond_scan_errors_total: counter,

    #[describe("Games inspected within the bond discovery window at the last scan tick")]
    bonds_inspected: gauge,

    #[describe("Bond candidates matching claim addresses at the last scan tick")]
    bond_candidates: gauge,

    #[describe("Bond workers currently running")]
    bond_workers_in_flight: gauge,

    #[describe("Bond worker total lifetime in seconds")]
    bond_worker_duration_seconds: histogram,

    #[describe("Bond pipeline step duration in seconds")]
    #[label(
        name = "step",
        default = ["resolve", "claim_unlocked", "wait_weth", "claim_withdrawn", "close"]
    )]
    bond_step_duration_seconds: histogram,

    #[describe("Game workers currently blocked acquiring a detect semaphore permit")]
    detect_semaphore_waiters: gauge,

    #[describe("Time spent waiting to acquire a detect semaphore permit, in seconds")]
    detect_semaphore_wait_seconds: histogram,
}

impl Metrics {
    /// Label value identifying the TEE prover.
    pub const PROOF_KIND_TEE: &'static str = "tee";

    /// Label value identifying the ZK prover.
    pub const PROOF_KIND_ZK: &'static str = "zk";

    /// Label value for a successful proof generation.
    pub const PROOF_STATUS_OK: &'static str = "ok";

    /// Label value for a failed proof generation.
    pub const PROOF_STATUS_FAIL: &'static str = "fail";

    /// Label value for the bond `resolve` step.
    pub const STEP_RESOLVE: &'static str = "resolve";

    /// Label value for the `claimCredit` step that unlocks the bond.
    pub const STEP_CLAIM_UNLOCKED: &'static str = "claim_unlocked";

    /// Label value for the `DelayedWETH` withdrawal delay wait.
    pub const STEP_WAIT_WETH: &'static str = "wait_weth";

    /// Label value for the `claimCredit` step that withdraws the bond.
    pub const STEP_CLAIM_WITHDRAWN: &'static str = "claim_withdrawn";

    /// Label value for the `closeGame` step.
    pub const STEP_CLOSE: &'static str = "close";

    /// Sets `up` to 1.
    pub fn record_startup() {
        Self::up().set(1.0);
    }
}
