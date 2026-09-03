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
    #[describe("Unsafe L2 head reported by the sequencer RPC")]
    #[no_zero]
    sequencer_unsafe_l2_block: gauge,
    #[describe("Safe L2 head reported by the sequencer RPC")]
    #[no_zero]
    sequencer_safe_l2_block: gauge,
    #[describe("Unsafe L2 head reported by the shadow parity validator RPC")]
    #[no_zero]
    validator_unsafe_l2_block: gauge,
    #[describe("Safe L2 head reported by the shadow parity validator RPC")]
    #[no_zero]
    validator_safe_l2_block: gauge,
    #[describe("Blocks the validator unsafe L2 head trails the sequencer unsafe L2 head")]
    #[no_zero]
    validator_unsafe_lag_blocks: gauge,
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
    #[describe(
        "Total errors that prevent a parity pass or block comparison from completing, by scope"
    )]
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
    /// An error aborted the pass before it compared any block.
    pub const SCOPE_PASS: &'static str = "pass";

    /// An error aborted one block comparison, leaving the cursor parked on that height.
    pub const SCOPE_BLOCK: &'static str = "block";
}
