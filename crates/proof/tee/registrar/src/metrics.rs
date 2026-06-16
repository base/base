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

    #[describe("Number of proof-generation tasks currently in-flight in the run() loop")]
    proof_tasks_pending: gauge,
}
