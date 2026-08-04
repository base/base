//! Action tests for the batcher gap-filling invariant.
//!
//! When the batcher is repointed between L2 nodes with different safe heads,
//! it must properly reset its encoder and start submitting from the current
//! safe head. This mirrors the production batcher flow that starts fresh and
//! clears channel manager state after a source divergence.
//!
//! The core invariant: **the batcher always submits blocks starting from
//! `safe_head + 1`**, regardless of what it was previously posting.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_protocol::BlockInfo;

// ---------------------------------------------------------------------------
// A. Gap-filling with a single persistent batcher (reorg signal path)
// ---------------------------------------------------------------------------

/// Verifies the batcher gap-filling invariant using a single persistent
/// [`Batcher`] instance that is "repointed" between nodes via
/// [`signal_reorg`].
///
/// Scenario (maps to the batcher's source-divergence handling):
///
/// 1. **Phase 1** — Batcher at node A (safe head 0 → 5):
///    Posts blocks 1-5. Verifier derives them; safe head advances to 5.
///
/// 2. **Phase 2** — Batcher repointed to node B (safe head 7):
///    [`signal_reorg`] clears the encoder. Batcher posts blocks 8-10.
///    These land on L1 but the verifier **cannot** derive them because
///    blocks 6-7 are missing (parent-hash mismatch against safe head 5).
///
/// 3. **Phase 3** — Batcher repointed back to node A (safe head 5):
///    [`signal_reorg`] clears the encoder again. Batcher posts blocks
///    6-10 (from `safe_head + 1 = 6` through `unsafe_head = 10`),
///    filling the gap. The verifier derives all remaining blocks;
///    safe head reaches 10.
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

    // ----- Phase 2: repoint to node B (safe head 7), post blocks 8-10 -----
    // signal_reorg clears the encoder, modelling the batcher detecting that
    // its block source has switched to a different chain position.
    batcher.signal_reorg().await;

    for block in &blocks[7..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    // The verifier should NOT advance past 5: batches for 8-10 have
    // parent_hash = hash(block 7) which doesn't match safe_head hash(block 5).
    let derived = node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        5,
        "Phase 2: safe head must remain at 5 — gap blocks 6-7 are missing"
    );
    assert_eq!(derived, 0, "Phase 2: no new blocks should be derived");

    // ----- Phase 3: repoint back to node A (safe head 5), fill the gap -----
    // In production, the batcher queries safe_head = 5 and loads blocks
    // [6, unsafe_head]. signal_reorg clears the encoder so we start fresh.
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
// B. Driver reset with safe-head tracking
// ---------------------------------------------------------------------------

/// Exercises driver and encoder recovery with a safe-head feed. This harness
/// replays missing blocks explicitly; numbered polling is tested in the source
/// crate.
#[tokio::test]
async fn batcher_gap_fill_with_safe_head_tracking() {
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

    // Wire a safe-head event channel into the batcher.
    let (safe_head_tx, safe_head_rx) = tokio::sync::mpsc::channel(1);
    let mut batcher = Batcher::with_safe_head_rx(
        ActionL2Source::new(),
        &h.rollup_config,
        batcher_cfg.clone(),
        BlockInfo::default(),
        safe_head_rx,
    );

    // ----- Phase 1: post blocks 1-5 -----
    for block in &blocks[..5] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 1: expected 5 L2 blocks derived");
    assert_eq!(node.l2_safe_number(), 5, "Phase 1: safe head must be 5");

    // Update the batcher's safe head to match the verifier.
    safe_head_tx.send(BlockInfo::from(&blocks[4])).await.expect("safe-head channel open");
    // Yield to let the driver process the safe-head update (prune_safe).
    tokio::task::yield_now().await;

    // ----- Phase 2: repoint to node B, post gap blocks 8-10 -----
    batcher.signal_reorg().await;

    for block in &blocks[7..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(node.l2_safe_number(), 5, "Phase 2: safe head must stay at 5");
    assert_eq!(derived, 0, "Phase 2: no blocks derived (gap)");

    // ----- Phase 3: repoint back, fill the gap from safe_head + 1 = 6 -----
    // The batcher's safe head remains 5, so catchup starts at 6.
    batcher.signal_reorg().await;

    for block in &blocks[5..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 3: expected 5 L2 blocks derived (6-10)");
    assert_eq!(node.l2_safe_number(), 10, "Phase 3: safe head must reach 10");
}

// ---------------------------------------------------------------------------
// C. Gap-filling with separate batcher instances (restart model)
// ---------------------------------------------------------------------------

/// Verifies the same gap-filling invariant using separate [`Batcher`]
/// instances, modelling the scenario where the batcher process is
/// restarted (or a fresh `channelManager.Clear()` equivalent) each time
/// it is repointed to a different node.
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

    // ----- Phase 1: batcher at node A posts blocks 1-5 -----
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

    // ----- Phase 2: batcher at node B posts blocks 8-10 (gap) -----
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

    // ----- Phase 3: batcher back at node A posts blocks 6-10 -----
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
