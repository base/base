/// Gauge: proposer build info, labelled with `version`.
pub const INFO: &str = "base_proposer_info";

/// Gauge: proposer is running (set to 1 at startup).
pub const UP: &str = "base_proposer_up";

/// Counter: total number of L2 output proposals submitted.
pub const L2_OUTPUT_PROPOSALS_TOTAL: &str = "base_proposer_l2_output_proposals_total";

/// Gauge: proposer account balance in wei.
pub const ACCOUNT_BALANCE_WEI: &str = "base_proposer_account_balance_wei";

/// Label key for version.
pub const LABEL_VERSION: &str = "version";

/// Records startup metrics (INFO gauge with version label, UP gauge set to 1).
pub fn record_startup_metrics(version: &str) {
    metrics::gauge!(INFO, LABEL_VERSION => version.to_string()).set(1.0);
    metrics::gauge!(UP).set(1.0);
}
