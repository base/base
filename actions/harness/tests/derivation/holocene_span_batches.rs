//! Action tests for span-batch derivation before and after Holocene.

use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_protocol::BatchType;

/// Shared setup helpers for Holocene span-batch action tests.
#[derive(Debug)]
struct HoloceneSpanFixture;

impl HoloceneSpanFixture {
    /// Returns a calldata span-batch configuration for deterministic action tests.
    fn span_batcher_config() -> BatcherConfig {
        BatcherConfig {
            batch_type: BatchType::Span,
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..BatcherConfig::default()
        }
    }

    /// Returns a calldata singular-batch configuration for mixed-format tests.
    fn singular_batcher_config() -> BatcherConfig {
        BatcherConfig {
            batch_type: BatchType::Single,
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..BatcherConfig::default()
        }
    }

    /// Returns a harness with span batches enabled but Holocene inactive.
    fn pre_holocene_harness(batcher: &BatcherConfig) -> ActionTestHarness {
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(batcher).through_granite().build();
        ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg)
    }

    /// Returns a harness with Holocene active from genesis.
    fn post_holocene_harness(batcher: &BatcherConfig) -> ActionTestHarness {
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(batcher).through_holocene().build();
        ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg)
    }
}

/// Post-Holocene span derivation accepts a valid multi-block span batch.
#[tokio::test]
async fn post_holocene_multi_block_span_derives() {
    const BLOCK_COUNT: u64 = 3;

    let batcher_cfg = HoloceneSpanFixture::span_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg, blocks).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, BLOCK_COUNT as usize, "all span blocks should derive");
    assert_eq!(node.l2_safe_number(), BLOCK_COUNT, "safe head should reach the final span block");
}

/// Post-Holocene span derivation accepts a valid span batch crossing an L1 epoch boundary.
#[tokio::test]
async fn post_holocene_span_crossing_l1_epoch_boundary_derives() {
    const BLOCK_COUNT: u64 = 6;

    let batcher_cfg = HoloceneSpanFixture::span_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    harness.mine_l1_blocks(1);
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;
    assert_eq!(sequencer.head().l1_origin.number, 1, "last block must reference L1 block 1");

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg, blocks).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, BLOCK_COUNT as usize, "all cross-epoch span blocks should derive");
    assert_eq!(node.l2_safe_number(), BLOCK_COUNT, "safe head should cross the L1 epoch boundary");
}

/// Post-Holocene span derivation rejects a span whose L1 origin hash points to
/// an orphaned L1 fork.
#[tokio::test]
async fn post_holocene_stale_span_l1_origin_check_after_reorg_is_rejected() {
    const BLOCK_COUNT: u64 = 6;

    let batcher_cfg = HoloceneSpanFixture::span_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    harness.mine_l1_blocks(1);
    let old_l1_1_hash = harness.l1.block_by_number(1).expect("old L1 block 1").hash();
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;
    assert_eq!(sequencer.head().l1_origin.number, 1, "stale span must reference L1 block 1");

    harness.l1.reorg_to(0).expect("reorg to genesis");
    harness.l1.mine_block();
    let new_l1_1_hash = harness.l1.block_by_number(1).expect("new L1 block 1").hash();
    assert_ne!(old_l1_1_hash, new_l1_1_hash, "replacement L1 block must have a new hash");

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg, blocks).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 0, "stale span origin check must be rejected");
    assert_eq!(node.l2_safe_number(), 0, "stale span must not advance safe head");
}

/// Pre-Holocene `BatchQueue` buffers a future span batch and derives it after the gap is filled.
#[tokio::test]
async fn pre_holocene_future_span_is_buffered_by_batch_queue() {
    let batcher_cfg = HoloceneSpanFixture::span_batcher_config();
    let mut harness = HoloceneSpanFixture::pre_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future pre-Holocene span should wait for the missing parent");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_1]).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "pre-Holocene BatchQueue should derive the new and buffered spans");
    assert_eq!(node.l2_safe_number(), 2, "safe head should include the buffered future span");
}

/// Post-Holocene strict ordering drops a future span batch instead of buffering it.
#[tokio::test]
async fn post_holocene_future_span_is_dropped_not_buffered() {
    let batcher_cfg = HoloceneSpanFixture::span_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future post-Holocene span should be dropped");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "only the expected next span should derive");
    assert_eq!(node.l2_safe_number(), 1, "future span must not have been buffered");

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_2]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "resubmitted span should derive after its parent is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance after resubmission");
}

/// Post-Holocene strict ordering also drops future singular batches instead of buffering them.
#[tokio::test]
async fn post_holocene_future_singular_is_dropped_not_buffered() {
    let batcher_cfg = HoloceneSpanFixture::singular_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future post-Holocene singular batch should be dropped");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "only the expected next singular batch should derive");
    assert_eq!(node.l2_safe_number(), 1, "future singular batch must not have been buffered");

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_2]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "resubmitted singular batch should derive after its parent is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance after resubmission");
}

/// Post-Holocene past singular batches are ignored without flushing following batches.
#[tokio::test]
async fn post_holocene_past_singular_does_not_flush_channel() {
    let batcher_cfg = HoloceneSpanFixture::singular_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1.clone()]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "block 1 should derive before the stale replay");
    assert_eq!(node.l2_safe_number(), 1);

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_1, block_2]).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "stale block 1 should be ignored and block 2 should still derive");
    assert_eq!(node.l2_safe_number(), 2, "post-Holocene past batches must not flush the channel");
}

/// Pre-Holocene past singular replays are ignored without poisoning following batches.
#[tokio::test]
async fn pre_holocene_past_singular_does_not_poison_channel() {
    let batcher_cfg = HoloceneSpanFixture::singular_batcher_config();
    let mut harness = HoloceneSpanFixture::pre_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1.clone()]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "block 1 should derive before the stale replay");
    assert_eq!(node.l2_safe_number(), 1);

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1, block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "stale block 1 should be ignored and block 2 should still derive");
    assert_eq!(node.l2_safe_number(), 2);
}

/// Post-Holocene derivation accepts singular and span batches in the same L1 stream.
#[tokio::test]
async fn post_holocene_mixed_singular_and_span_batches_derive() {
    let span_cfg = HoloceneSpanFixture::span_batcher_config();
    let singular_cfg = HoloceneSpanFixture::singular_batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&span_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, singular_cfg, vec![block_1]).await;
    harness.submit_l2_blocks(&chain, span_cfg, vec![block_2]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "singular and span batches should both derive");
    assert_eq!(node.l2_safe_number(), 2, "safe head should include both batch formats");
}
