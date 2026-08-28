//! Batcher service metric definitions.

base_metrics::define_metrics! {
    batcher.l2_block_parity, struct = L2BlockParityMetrics,
    #[describe("Whether derived L2 block parity monitoring is running")]
    enabled: gauge,
    #[describe("Whether a derived L2 block hash mismatch has been observed since process start")]
    divergence_detected: gauge,
    #[describe("L2 block this monitor run started comparing from")]
    #[no_zero]
    start_l2_block: gauge,
    #[describe("Latest L2 block reported by the sequencer RPC")]
    #[no_zero]
    sequencer_latest_l2_block: gauge,
    #[describe("Latest L2 block reported by the shadow parity validator RPC")]
    #[no_zero]
    validator_latest_l2_block: gauge,
    #[describe("Current sequencer-to-validator L2 block lag")]
    #[no_zero]
    lag_blocks: gauge,
    #[describe("Comparable derived L2 blocks not compared yet")]
    #[no_zero]
    verification_backlog_blocks: gauge,
    #[describe("Total derived L2 blocks compared")]
    checked_total: counter,
    #[describe("Total derived L2 block hash matches")]
    matches_total: counter,
    #[describe("Total derived L2 block hash mismatches")]
    mismatches_total: counter,
    #[describe("Total derived L2 blocks skipped because one side did not return the block")]
    missing_blocks_total: counter,
    #[describe("Total RPC fetch errors seen by derived L2 block parity monitoring")]
    #[label(name = "scope", default = ["pass", "block"])]
    fetch_errors_total: counter,
    #[describe("Latest L2 block compared by derived block parity monitoring")]
    #[no_zero]
    last_checked_l2_block: gauge,
    #[describe("Latest derived L2 block where parity matched")]
    #[no_zero]
    last_match_l2_block: gauge,
    #[describe("Latest derived L2 block where parity mismatched")]
    #[no_zero]
    last_mismatch_l2_block: gauge,
}

impl L2BlockParityMetrics {
    /// Chain-head fetch failed, so the pass compared nothing.
    pub const SCOPE_PASS: &'static str = "pass";

    /// Block fetch failed mid-pass, leaving the cursor parked on that height.
    pub const SCOPE_BLOCK: &'static str = "block";
}
