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

    #[describe("Total number of on-chain durable revocation pre-checks performed")]
    onchain_revocation_checks_total: counter,

    #[describe("Total number of intermediates rejected by the on-chain durable revocation sentinel")]
    onchain_revocations_detected: counter,

    #[describe("Total number of on-chain revocation pre-checks that failed and fell through to the AWS CRL layer (fail-open)")]
    onchain_revocation_check_errors: counter,

    #[describe("Total number of revokeCert transaction submission failures")]
    revoke_cert_tx_failures: counter,

    #[describe("Total number of successful revokeCert transactions")]
    revoke_cert_success_total: counter,

    #[describe("Registrar L1 account balance in wei")]
    account_balance_wei: gauge,

    #[describe("Registrar Boundless account balance in wei")]
    boundless_balance_wei: gauge,
}

impl RegistrarMetrics {
    /// Records shutdown by setting the UP gauge to 0.
    pub fn record_shutdown() {
        Self::up().set(0.0);
    }
}
