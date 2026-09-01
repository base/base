//! Action tests for derivation across gaps in submitted L2 blocks.
//!
//! Batches submitted after a gap cannot advance the verifier's safe head.
//! Submitting the missing sequence fills the gap, and duplicate later blocks
//! remain harmless.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

// ---------------------------------------------------------------------------
// A. Gap-filling with a single persistent batcher (reorg signal path)
// ---------------------------------------------------------------------------

/// Verifies gap filling with a persistent [`Batcher`] whose encoder is reset
/// between block sequences via [`signal_reorg`].
///
/// Scenario (maps to the batcher's source-divergence handling):
///
/// 1. **Phase 1** — Posts blocks 1-5. The verifier derives them and advances
///    its safe head to 5.
///
/// 2. **Phase 2** — [`signal_reorg`] clears the encoder, then the batcher posts
///    blocks 8-10.
///    These land on L1 but the verifier **cannot** derive them because
///    blocks 6-7 are missing. Block 8's timestamp is ahead of the next expected
///    slot (block 6), so it is classified as future.
///
/// 3. **Phase 3** — [`signal_reorg`] clears the encoder again, then the batcher
///    posts blocks 6-10. The verifier derives all remaining blocks and reaches
///    safe head 10.
///
/// This tests:
/// - Encoder reset on reorg signal (no stale state leaks between repoints)
/// - Out-of-order batches on L1 do not advance the safe head
/// - Gap-filling batches allow the verifier to derive past the gap
/// - Duplicate batches (8-10 posted in both Phase 2 and Phase 3) are harmless
///
/// [`signal_reorg`]: Batcher::signal_reorg
#[tokio::test]
async fn batcher_gap_fill_single_instance_reorg_signal() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 10 L2 blocks up front so they share a consistent chain.
    let mut blocks = Vec::with_capacity(10);
    for _ in 0..10 {
        blocks.push(sequencer.build_next_block_with_single_transaction().await);
    }

    // Create the verifier node before any mining so `chain.push` makes
    // subsequent L1 blocks visible to the derivation pipeline.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Create a single batcher that persists across all phases.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());

    // ----- Phase 1: post blocks 1-5, derive them -----
    for block in &blocks[..5] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 1: expected 5 L2 blocks derived");
    assert_eq!(node.l2_safe_number(), 5, "Phase 1: safe head must be 5");

    // ----- Phase 2: reset the encoder, then post blocks 8-10 -----
    batcher.signal_reorg().await;

    for block in &blocks[7..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    // The verifier should NOT advance past 5: block 8's timestamp is ahead of the next expected
    // slot (block 6), so it is classified as future.
    let derived = node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        5,
        "Phase 2: safe head must remain at 5 — gap blocks 6-7 are missing"
    );
    assert_eq!(derived, 0, "Phase 2: no new blocks should be derived");

    // ----- Phase 3: reset the encoder, then fill the gap with blocks 6-10 -----
    batcher.signal_reorg().await;

    // Post blocks 6-10: fills the gap (6-7) and re-posts 8-10.
    for block in &blocks[5..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 3: expected 5 L2 blocks derived (6-10)");
    assert_eq!(node.l2_safe_number(), 10, "Phase 3: safe head must reach 10 after gap is filled");
}

// ---------------------------------------------------------------------------
// B. Gap-filling with separate batcher instances (restart model)
// ---------------------------------------------------------------------------

/// Verifies the same derivation behavior using separate [`Batcher`] instances,
/// modelling a fresh encoder for each submitted block sequence.
///
/// Each `Batcher` instance starts with a clean [`BatchEncoder`], which
/// is the state that results from the batcher's fresh-start path.
///
/// [`BatchEncoder`]: base_batcher_encoder::BatchEncoder
#[tokio::test]
async fn batcher_gap_fill_separate_instances() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::with_capacity(10);
    for _ in 0..10 {
        blocks.push(sequencer.build_next_block_with_single_transaction().await);
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // ----- Phase 1: post blocks 1-5 -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[..5] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 1: expected 5 L2 blocks derived");
    assert_eq!(node.l2_safe_number(), 5, "Phase 1: safe head must be 5");

    // ----- Phase 2: post blocks 8-10 with a fresh batcher (gap) -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[7..10] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    let derived = node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        5,
        "Phase 2: safe head must remain at 5 — gap blocks 6-7 are missing"
    );
    assert_eq!(derived, 0, "Phase 2: no blocks derived (gap)");

    // ----- Phase 3: post blocks 6-10 with a fresh batcher -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[5..10] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 3: expected 5 L2 blocks derived (6-10)");
    assert_eq!(node.l2_safe_number(), 10, "Phase 3: safe head must reach 10 after gap is filled");
}
