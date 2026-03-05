/// Challenger metrics helpers.
#[derive(Debug)]
pub struct ChallengerMetrics;

impl ChallengerMetrics {
    /// Gauge: challenger build info, labelled with `version`.
    pub const INFO: &str = "base_challenger_info";

    /// Gauge: challenger is running (set to 1 at startup).
    pub const UP: &str = "base_challenger_up";

    /// Counter: total number of games evaluated during scanning.
    pub const GAMES_SCANNED_TOTAL: &str = "base_challenger_games_scanned_total";

    /// Gauge: latest factory index scanned by the game scanner.
    pub const SCAN_HEAD: &str = "base_challenger_scan_head";

    /// Counter: total number of games found to be invalid during validation.
    pub const GAMES_INVALID_TOTAL: &str = "base_challenger_games_invalid_total";

    /// Counter: total number of validation errors (RPC failures, header mismatches, etc.).
    pub const VALIDATION_ERRORS_TOTAL: &str = "base_challenger_validation_errors_total";

    /// Histogram: latency in seconds for output root validation.
    pub const VALIDATION_LATENCY_SECONDS: &str = "base_challenger_validation_latency_seconds";

    /// Label key for version.
    pub const LABEL_VERSION: &str = "version";

    /// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
    pub fn record_startup(version: &str) {
        metrics::gauge!(Self::INFO, Self::LABEL_VERSION => version.to_string()).set(1.0);
        metrics::gauge!(Self::UP).set(1.0);
    }
}
