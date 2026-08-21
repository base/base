//! Batcher service metric definitions.

base_metrics::define_metrics! {
    batcher.l2_block_parity, struct = L2BlockParityMetrics,
    #[describe("Whether derived L2 block parity monitoring is running")]
    enabled: gauge,
    #[describe("Latest L2 block reported by the sequencer RPC")]
    sequencer_latest_l2_block: gauge,
    #[describe("Latest L2 block reported by the shadow parity validator RPC")]
    validator_latest_l2_block: gauge,
    #[describe("Current sequencer-to-validator L2 block lag")]
    lag_blocks: gauge,
    #[describe("Total derived L2 blocks compared")]
    checked_total: counter,
    #[describe("Total derived L2 block hash matches")]
    matches_total: counter,
    #[describe("Total derived L2 block hash mismatches")]
    mismatches_total: counter,
    #[describe("Total derived L2 blocks skipped because one side did not return the block")]
    missing_blocks_total: counter,
    #[describe("Total RPC fetch errors seen by derived L2 block parity monitoring")]
    fetch_errors_total: counter,
    #[describe("Latest derived L2 block parity alignment state: 1 for aligned, 0 for mismatch or lag")]
    aligned: gauge,
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
