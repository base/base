//! Registrar metrics constants.

use crate::CertKind;

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

    #[describe("Total number of signer-registration tasks spawned by the run() loop")]
    proof_tasks_spawned: counter,

    #[describe("Total number of signer-registration tasks the run() loop intentionally cancelled (vanished/ineligible instances or shutdown). Records the cancel intent; the task still terminates as a `completed` outcome.")]
    proof_tasks_cancelled: counter,

    #[describe("Total number of signer-registration tasks that ran to terminal state (success, error, panic, or cooperative cancellation)")]
    proof_tasks_completed: counter,

    #[describe("Total number of signer-registration tasks that ran to terminal state by outcome")]
    #[label(name = "outcome", default = ["succeeded", "failed", "cancelled", "join_error"])]
    proof_tasks_completed_total: counter,

    #[describe("Number of signer-registration tasks currently in-flight in the run() loop")]
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

    #[describe("Total hint-generation attempts by outcome")]
    #[label(name = "outcome", default = ["started", "succeeded", "failed", "cancelled"])]
    hint_generation_total: counter,

    #[describe("Generated P-384 inverse-hint stream size in bytes")]
    #[label(name = "kind", default = ["ca", "leaf", "attestation"])]
    hint_size_bytes: histogram,

    #[describe("Certificate cache lookups by kind and outcome")]
    #[label(name = "kind", default = ["ca", "leaf"])]
    #[label(name = "outcome", default = ["hit", "miss"])]
    cert_cache_lookup_total: counter,

    #[describe("Certificate cache transactions by kind and outcome")]
    #[label(name = "kind", default = ["ca", "leaf"])]
    #[label(name = "outcome", default = ["submitted", "succeeded", "reverted", "retry", "failed", "observed_cached"])]
    cert_cache_tx_total: counter,

    #[describe("Recoveries from ambiguous onchain state after a cache or final-registration transaction")]
    #[label(name = "kind", default = ["cache", "final"])]
    registration_recovery_total: counter,

    #[describe("Final hinted registration transactions by outcome")]
    #[label(name = "outcome", default = ["submitted", "succeeded", "reverted", "retry", "failed", "observed_registered", "stale"])]
    final_registration_total: counter,
}

impl RegistrarMetrics {
    /// Signer-registration task completed successfully.
    pub const PROOF_TASK_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Signer-registration task completed with an error.
    pub const PROOF_TASK_OUTCOME_FAILED: &'static str = "failed";
    /// Signer-registration task completed after cooperative cancellation.
    pub const PROOF_TASK_OUTCOME_CANCELLED: &'static str = "cancelled";
    /// Signer-registration task failed to join because it panicked or was aborted.
    pub const PROOF_TASK_OUTCOME_JOIN_ERROR: &'static str = "join_error";

    /// Signer was already registered before this task started hint generation.
    pub const REGISTRATION_STAGE_ALREADY_REGISTERED: &'static str = "already_registered";
    /// Registrar started hint generation for a signer.
    pub const REGISTRATION_STAGE_PROOF_STARTED: &'static str = "proof_started";
    /// Hint generation completed successfully.
    pub const REGISTRATION_STAGE_PROOF_SUCCEEDED: &'static str = "proof_succeeded";
    /// Hint generation failed.
    pub const REGISTRATION_STAGE_PROOF_FAILED: &'static str = "proof_failed";
    /// Hint generation was cancelled.
    pub const REGISTRATION_STAGE_PROOF_CANCELLED: &'static str = "proof_cancelled";
    /// Registration material failed local validation before transaction submission.
    pub const REGISTRATION_STAGE_PROOF_INVALID: &'static str = "proof_invalid";
    /// Registration material became stale before transaction submission.
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

    /// Records a signer-registration task's terminal outcome.
    pub fn record_proof_task_completed(outcome: &'static str) {
        Self::proof_tasks_completed_total(outcome).increment(1);
    }

    /// Records a registration lifecycle stage.
    pub fn record_registration_stage(stage: &'static str) {
        Self::registration_stage_total(stage).increment(1);
    }

    /// Bounded label for a certificate-cache kind.
    pub const CERT_KIND_CA: &'static str = "ca";
    /// Bounded label for a leaf certificate.
    pub const CERT_KIND_LEAF: &'static str = "leaf";
    /// Bounded label for the attestation signature hint stream.
    pub const HINT_KIND_ATTESTATION: &'static str = "attestation";

    /// Hint generation started.
    pub const HINT_GENERATION_STARTED: &'static str = "started";
    /// Hint generation completed successfully.
    pub const HINT_GENERATION_SUCCEEDED: &'static str = "succeeded";
    /// Hint generation failed.
    pub const HINT_GENERATION_FAILED: &'static str = "failed";
    /// Hint generation was cancelled.
    pub const HINT_GENERATION_CANCELLED: &'static str = "cancelled";

    /// Cache lookup found a usable cached certificate.
    pub const CACHE_LOOKUP_HIT: &'static str = "hit";
    /// Cache lookup found no usable cached certificate.
    pub const CACHE_LOOKUP_MISS: &'static str = "miss";

    /// Cache or final-registration transaction was submitted.
    pub const TX_OUTCOME_SUBMITTED: &'static str = "submitted";
    /// Transaction succeeded and produced the expected onchain state.
    pub const TX_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Transaction was included but reverted.
    pub const TX_OUTCOME_REVERTED: &'static str = "reverted";
    /// Registrar scheduled a retry after a retryable submission failure.
    pub const TX_OUTCOME_RETRY: &'static str = "retry";
    /// Transaction submission failed permanently.
    pub const TX_OUTCOME_FAILED: &'static str = "failed";
    /// Certificate was observed cached after an ambiguous cache transaction.
    pub const TX_OUTCOME_OBSERVED_CACHED: &'static str = "observed_cached";
    /// Signer was observed registered after an ambiguous final transaction.
    pub const TX_OUTCOME_OBSERVED_REGISTERED: &'static str = "observed_registered";
    /// Final registration was abandoned because the attestation became stale.
    pub const TX_OUTCOME_STALE: &'static str = "stale";

    /// Recovery after an ambiguous certificate-cache transaction.
    pub const RECOVERY_CACHE: &'static str = "cache";
    /// Recovery after an ambiguous final-registration transaction.
    pub const RECOVERY_FINAL: &'static str = "final";

    /// Returns the bounded cache-kind label for `kind`.
    pub const fn cert_kind_label(kind: CertKind) -> &'static str {
        match kind {
            CertKind::Ca => Self::CERT_KIND_CA,
            CertKind::Leaf => Self::CERT_KIND_LEAF,
        }
    }

    /// Records a hint-generation outcome.
    pub fn record_hint_generation(outcome: &'static str) {
        Self::hint_generation_total(outcome).increment(1);
    }

    /// Records one hint-stream size sample.
    pub fn record_hint_size(kind: &'static str, size_bytes: usize) {
        Self::hint_size_bytes(kind).record(size_bytes as f64);
    }

    /// Records a certificate-cache lookup.
    pub fn record_cache_lookup(kind: CertKind, hit: bool) {
        Self::cert_cache_lookup_total(
            Self::cert_kind_label(kind),
            if hit { Self::CACHE_LOOKUP_HIT } else { Self::CACHE_LOOKUP_MISS },
        )
        .increment(1);
    }

    /// Records a certificate-cache transaction outcome.
    pub fn record_cache_tx(kind: CertKind, outcome: &'static str) {
        Self::cert_cache_tx_total(Self::cert_kind_label(kind), outcome).increment(1);
    }

    /// Records a recovery from ambiguous onchain state.
    pub fn record_recovery(kind: &'static str) {
        Self::registration_recovery_total(kind).increment(1);
    }

    /// Records a final hinted-registration transaction outcome.
    pub fn record_final_registration(outcome: &'static str) {
        Self::final_registration_total(outcome).increment(1);
    }
}
