//! Challenger metrics constants and startup recording.

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

    /// Counter: total number of nullify transactions submitted.
    pub const NULLIFY_TX_SUBMITTED_TOTAL: &str = "base_challenger_nullify_tx_submitted_total";

    /// Counter: total number of nullify transaction outcomes (labelled by status).
    pub const NULLIFY_TX_OUTCOME_TOTAL: &str = "base_challenger_nullify_tx_outcome_total";

    /// Histogram: latency in seconds for nullify transaction confirmation.
    pub const NULLIFY_TX_LATENCY_SECONDS: &str = "base_challenger_nullify_tx_latency_seconds";

    /// Counter: total number of proof retries after failure.
    pub const PROOF_RETRIES_TOTAL: &str = "base_challenger_proof_retries_total";

    /// Gauge: number of in-flight proof sessions.
    pub const PENDING_PROOFS: &str = "base_challenger_pending_proofs";

    /// Label key for status.
    pub const LABEL_STATUS: &str = "status";

    /// Label value for a successfully confirmed transaction.
    pub const STATUS_SUCCESS: &str = "success";

    /// Label value for a reverted transaction.
    pub const STATUS_REVERTED: &str = "reverted";

    /// Label value for a transaction that failed to send.
    pub const STATUS_ERROR: &str = "error";

    /// Label key for version.
    pub const LABEL_VERSION: &str = "version";

    /// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
    pub fn record_startup(version: &str) {
        metrics::gauge!(Self::INFO, Self::LABEL_VERSION => version.to_string()).set(1.0);
        metrics::gauge!(Self::UP).set(1.0);
    }
}
