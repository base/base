//! Proposer metrics.

base_metrics::define_metrics! {
    base_proposer
    #[describe("Proposer is running")]
    up: gauge,

    #[describe("Proposer account balance in wei")]
    account_balance_wei: gauge,

    #[describe("Most recently proposed L2 block number")]
    last_proposed_block: gauge,

    #[describe("Proof tasks currently in flight")]
    inflight_proofs: gauge,

    #[describe("Proved results awaiting sequential submission")]
    proved_queue_depth: gauge,

    #[describe("Total pending retries across all target blocks")]
    pipeline_retries: gauge,

    #[describe("Latest safe (or finalized) L2 block number")]
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
            "output_root_mismatch",
            "l1_origin_mismatch",
            "block_number_mismatch",
            "block_info_derivation",
            "tx_manager",
            "game_already_exists"
        ]
    )]
    errors_total: counter,

    #[describe("Total reorgs detected at submit time")]
    reorgs_total: counter,
    #[describe("Time to generate a single proof (seconds)")]
    proof_duration_seconds: histogram,

    #[describe("Time for one pipeline tick (seconds)")]
    tick_duration_seconds: histogram,

    #[describe("Time to validate and submit a proposal (seconds)")]
    submission_duration_seconds: histogram,
}

impl Metrics {
    /// RPC error.
    pub const ERROR_TYPE_RPC: &str = "rpc";
    /// Prover error.
    pub const ERROR_TYPE_PROVER: &str = "prover";
    /// Contract interaction error.
    pub const ERROR_TYPE_CONTRACT: &str = "contract";
    /// Transaction reverted on-chain.
    pub const ERROR_TYPE_TX_REVERTED: &str = "tx_reverted";
    /// Configuration error.
    pub const ERROR_TYPE_CONFIG: &str = "config";
    /// Internal error.
    pub const ERROR_TYPE_INTERNAL: &str = "internal";
    /// Output root mismatch.
    pub const ERROR_TYPE_OUTPUT_ROOT_MISMATCH: &str = "output_root_mismatch";
    /// L1 origin hash mismatch.
    pub const ERROR_TYPE_L1_ORIGIN_MISMATCH: &str = "l1_origin_mismatch";
    /// Block number mismatch.
    pub const ERROR_TYPE_BLOCK_NUMBER_MISMATCH: &str = "block_number_mismatch";
    /// Block info derivation failure.
    pub const ERROR_TYPE_BLOCK_INFO_DERIVATION: &str = "block_info_derivation";
    /// Transaction manager error.
    pub const ERROR_TYPE_TX_MANAGER: &str = "tx_manager";
    /// Game already exists.
    pub const ERROR_TYPE_GAME_ALREADY_EXISTS: &str = "game_already_exists";
}

impl Metrics {
    pub(crate) fn record_startup() {
        Self::up().set(1.0);
    }
}
