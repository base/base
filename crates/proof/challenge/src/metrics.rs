//! Prometheus metric constants and startup recording for the challenger.

/// Gauge: challenger build info, labelled with `version`.
pub const INFO: &str = "base_challenger_info";

/// Gauge: challenger is running (set to 1 at startup).
pub const UP: &str = "base_challenger_up";

/// Label key for version.
pub const LABEL_VERSION: &str = "version";

/// Challenger metrics helpers.
#[derive(Debug)]
pub struct ChallengerMetrics;

impl ChallengerMetrics {
    /// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
    pub fn record_startup(version: &str) {
        metrics::gauge!(INFO, LABEL_VERSION => version.to_string()).set(1.0);
        metrics::gauge!(UP).set(1.0);
    }
}
