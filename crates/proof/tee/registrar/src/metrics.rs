//! Registrar metrics constants.

base_metrics::define_metrics! {
    base_registrar,
    struct = RegistrarMetrics,

    #[describe("Registrar is running")]
    up: gauge,

    #[describe("Total number of signer registrations submitted")]
    registrations_total: counter,

    #[describe("Total number of signer deregistrations submitted")]
    deregistrations_total: counter,

    #[describe("Total number of successful discovery cycles")]
    discovery_success_total: counter,

    #[describe("Total number of processing errors encountered")]
    processing_errors_total: counter,

    #[describe("Total number of CRL checks performed")]
    crl_checks_total: counter,

    #[describe("Total number of certificate revocations detected via CRL")]
    crl_revocations_detected: counter,

    #[describe("Total number of onchain durable revocation pre-checks performed")]
    onchain_revocation_checks_total: counter,

    #[describe("Total number of intermediates rejected by the onchain durable revocation sentinel")]
    onchain_revocations_detected: counter,

    #[describe("Total number of onchain revocation pre-checks that failed and fell through to the AWS CRL layer (fail-open)")]
    onchain_revocation_check_errors: counter,

    #[describe("Total number of revokeCert transaction submission failures")]
    revoke_cert_tx_failures: counter,

    #[describe("Total number of revokeCert transactions that landed onchain but reverted")]
    revoke_cert_reverted_total: counter,

    #[describe("Total number of successful revokeCert transactions")]
    revoke_cert_success_total: counter,

    #[describe("Registrar L1 account balance in wei")]
    account_balance_wei: gauge,

    #[describe("Registrar Boundless account balance in wei")]
    boundless_balance_wei: gauge,

    #[describe("Total number of proof-generation tasks spawned by the run() loop")]
    proof_tasks_spawned: counter,

    #[describe("Total number of proof-generation tasks the run() loop intentionally cancelled (vanished/ineligible instances or shutdown). Records the cancel intent; the task still terminates as a `completed` outcome.")]
    proof_tasks_cancelled: counter,

    #[describe("Total number of proof-generation tasks that ran to terminal state (success, error, panic, or cooperative cancellation)")]
    proof_tasks_completed: counter,

    #[describe("Total number of proof-generation tasks that ran to terminal state by outcome")]
    #[label(name = "outcome", default = ["succeeded", "failed", "cancelled", "join_error"])]
    proof_tasks_completed_total: counter,

    #[describe("Number of proof-generation tasks currently in-flight in the run() loop")]
    proof_tasks_pending: gauge,

    #[describe("Number of prover instances discovered in the latest successful discovery cycle")]
    discovered_instances_count: gauge,

    #[describe("Number of active signer addresses in the latest successful discovery cycle")]
    active_signers_count: gauge,

    #[describe("Number of signer addresses eligible for registration in the latest successful discovery cycle")]
    registerable_signers_count: gauge,

    #[describe("Number of unresolved prover instances in the latest successful discovery cycle")]
    unresolved_instances_count: gauge,

    #[describe("Total number of Registrar registration lifecycle stage observations")]
    #[label(name = "stage", default = ["already_registered", "proof_started", "proof_succeeded", "proof_failed", "proof_cancelled", "proof_invalid", "proof_stale", "tx_submitted", "tx_retry", "tx_succeeded", "tx_failed", "tx_reverted", "tx_observed_registered"])]
    registration_stage_total: counter,
}

impl RegistrarMetrics {
    /// Proof task completed successfully.
    pub const PROOF_TASK_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Proof task completed with an error.
    pub const PROOF_TASK_OUTCOME_FAILED: &'static str = "failed";
    /// Proof task completed after cooperative cancellation.
    pub const PROOF_TASK_OUTCOME_CANCELLED: &'static str = "cancelled";
    /// Proof task failed to join because it panicked or was aborted.
    pub const PROOF_TASK_OUTCOME_JOIN_ERROR: &'static str = "join_error";

    /// Signer was already registered before this task started proof generation.
    pub const REGISTRATION_STAGE_ALREADY_REGISTERED: &'static str = "already_registered";
    /// Registrar started Boundless proof generation for a signer.
    pub const REGISTRATION_STAGE_PROOF_STARTED: &'static str = "proof_started";
    /// Boundless proof generation completed successfully.
    pub const REGISTRATION_STAGE_PROOF_SUCCEEDED: &'static str = "proof_succeeded";
    /// Boundless proof generation failed.
    pub const REGISTRATION_STAGE_PROOF_FAILED: &'static str = "proof_failed";
    /// Boundless proof generation was cancelled.
    pub const REGISTRATION_STAGE_PROOF_CANCELLED: &'static str = "proof_cancelled";
    /// Generated proof failed local validation before transaction submission.
    pub const REGISTRATION_STAGE_PROOF_INVALID: &'static str = "proof_invalid";
    /// Generated proof became stale before transaction submission.
    pub const REGISTRATION_STAGE_PROOF_STALE: &'static str = "proof_stale";
    /// Registrar submitted a registration transaction candidate.
    pub const REGISTRATION_STAGE_TX_SUBMITTED: &'static str = "tx_submitted";
    /// Registrar scheduled a retry after a retryable transaction submission failure.
    pub const REGISTRATION_STAGE_TX_RETRY: &'static str = "tx_retry";
    /// Registration transaction succeeded.
    pub const REGISTRATION_STAGE_TX_SUCCEEDED: &'static str = "tx_succeeded";
    /// Registration transaction submission failed permanently.
    pub const REGISTRATION_STAGE_TX_FAILED: &'static str = "tx_failed";
    /// Registration transaction was included but reverted.
    pub const REGISTRATION_STAGE_TX_REVERTED: &'static str = "tx_reverted";
    /// Signer was observed registered after a transaction submission error.
    pub const REGISTRATION_STAGE_TX_OBSERVED_REGISTERED: &'static str = "tx_observed_registered";

    /// Records a proof-task terminal outcome.
    pub fn record_proof_task_completed(outcome: &'static str) {
        Self::proof_tasks_completed_total(outcome).increment(1);
    }

    /// Records a registration lifecycle stage.
    pub fn record_registration_stage(stage: &'static str) {
        Self::registration_stage_total(stage).increment(1);
    }
}
