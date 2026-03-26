//! Registrar metrics constants and startup recording.

/// Registrar metrics helpers.
#[derive(Debug)]
pub struct RegistrarMetrics;

impl RegistrarMetrics {
    /// Gauge: registrar build info, labelled with `version`.
    pub const INFO: &str = "base_registrar_info";

    /// Gauge: registrar is running (set to 1 at startup, 0 on shutdown).
    pub const UP: &str = "base_registrar_up";

    /// Counter: total number of signer registrations submitted.
    pub const REGISTRATIONS_TOTAL: &str = "base_registrar_registrations_total";

    /// Counter: total number of signer deregistrations submitted.
    pub const DEREGISTRATIONS_TOTAL: &str = "base_registrar_deregistrations_total";

    /// Counter: total number of successful discovery cycles.
    pub const DISCOVERY_SUCCESS_TOTAL: &str = "base_registrar_discovery_success_total";

    /// Counter: total number of processing errors encountered.
    pub const PROCESSING_ERRORS_TOTAL: &str = "base_registrar_processing_errors_total";

    /// Label key for version.
    pub const LABEL_VERSION: &str = "version";

    /// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
    pub fn record_startup(version: &str) {
        metrics::gauge!(Self::INFO, Self::LABEL_VERSION => version.to_string()).set(1.0);
        metrics::gauge!(Self::UP).set(1.0);
    }

    /// Records shutdown by setting the UP gauge to 0.
    pub fn record_shutdown() {
        metrics::gauge!(Self::UP).set(0.0);
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Expected prefix for all registrar metrics.
    const METRIC_PREFIX: &str = "base_registrar_";

    #[rstest]
    fn record_startup_does_not_panic() {
        RegistrarMetrics::record_startup("0.0.0-test");
    }

    #[rstest]
    #[case::info(RegistrarMetrics::INFO)]
    #[case::up(RegistrarMetrics::UP)]
    #[case::registrations(RegistrarMetrics::REGISTRATIONS_TOTAL)]
    #[case::deregistrations(RegistrarMetrics::DEREGISTRATIONS_TOTAL)]
    #[case::discovery(RegistrarMetrics::DISCOVERY_SUCCESS_TOTAL)]
    #[case::processing_errors(RegistrarMetrics::PROCESSING_ERRORS_TOTAL)]
    fn metric_names_follow_naming_convention(#[case] name: &str) {
        assert!(name.starts_with(METRIC_PREFIX), "{name} must start with {METRIC_PREFIX}");
    }

    #[rstest]
    #[case::registrations(RegistrarMetrics::REGISTRATIONS_TOTAL)]
    #[case::deregistrations(RegistrarMetrics::DEREGISTRATIONS_TOTAL)]
    #[case::discovery(RegistrarMetrics::DISCOVERY_SUCCESS_TOTAL)]
    #[case::processing_errors(RegistrarMetrics::PROCESSING_ERRORS_TOTAL)]
    fn counter_names_use_total_suffix(#[case] name: &str) {
        assert!(name.ends_with("_total"), "{name} must end with _total");
    }
}
