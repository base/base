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
}

impl RegistrarMetrics {
    /// Records shutdown by setting the UP gauge to 0.
    pub fn record_shutdown() {
        Self::up().set(0.0);
    }
}
