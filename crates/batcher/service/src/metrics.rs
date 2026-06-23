//! Batcher service metric definitions.

base_metrics::define_metrics! {
    batcher.shadow_parity, struct = BatcherServiceMetrics,
    #[describe("Whether the shadow DA parity monitor is running")]
    enabled: gauge,
    #[describe("Latest L1 block processed by the shadow DA parity monitor")]
    latest_l1_block: gauge,
    #[describe("Total canonical batch inbox payloads observed by the shadow DA parity monitor")]
    canonical_payloads_total: counter,
    #[describe("Total shadow batch inbox payloads observed by the shadow DA parity monitor")]
    shadow_payloads_total: counter,
    #[describe("Total canonical complete channels decoded by the shadow DA parity monitor")]
    canonical_complete_channels_total: counter,
    #[describe("Total shadow complete channels decoded by the shadow DA parity monitor")]
    shadow_complete_channels_total: counter,
    #[describe("Total canonical batches decoded by the shadow DA parity monitor")]
    canonical_batches_total: counter,
    #[describe("Total shadow batches decoded by the shadow DA parity monitor")]
    shadow_batches_total: counter,
    #[describe("Canonical decoded batches waiting for a shadow comparison")]
    canonical_pending_batches: gauge,
    #[describe("Shadow decoded batches waiting for a canonical comparison")]
    shadow_pending_batches: gauge,
    #[describe("Absolute decoded-batch queue length difference between canonical and shadow")]
    pending_batch_delta: gauge,
    #[describe("Total matching batch parity comparisons")]
    matches_total: counter,
    #[describe("Total diverging batch parity comparisons")]
    divergences_total: counter,
    #[describe("Latest shadow DA parity alignment state: 1 for aligned, 0 for divergence or lag")]
    aligned: gauge,
    #[describe("Latest L1 block where a matching shadow DA parity comparison was observed")]
    #[no_zero]
    last_match_l1_block: gauge,
    #[describe("Latest L1 block where a shadow DA parity divergence was observed")]
    #[no_zero]
    last_divergence_l1_block: gauge,
    #[describe("Total L1 fetch errors seen by the shadow DA parity monitor")]
    l1_fetch_errors_total: counter,
    #[describe("Total blob sidecar fetch errors seen by the shadow DA parity monitor")]
    blob_fetch_errors_total: counter,
    #[describe("Total payload/frame/channel extraction errors seen by the shadow DA parity monitor")]
    extraction_errors_total: counter,
    #[describe("Total incomplete channels evicted by the shadow DA parity monitor")]
    evicted_channels_total: counter,
    #[describe("Total decoded batches evicted by the shadow DA parity monitor")]
    evicted_batches_total: counter,
    #[describe("Total blob submissions skipped because no L1 beacon URL is configured")]
    missing_beacon_total: counter,
}

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
