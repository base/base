//! Action tests for the batcher gap-filling invariant.
//!
//! When the batcher is repointed between L2 nodes with different safe heads,
//! it must properly reset its encoder and start submitting from the current
//! safe head. This mirrors the production batcher flow that starts fresh and
//! clears channel manager state after a source divergence.
//!
//! The core invariant: **the batcher always submits blocks starting from
//! `safe_head + 1`**, regardless of what it was previously posting.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_protocol::{BlockInfo, L2BlockInfo};

/// Helper to construct a minimal [`L2BlockInfo`] for [`Batcher::signal_reorg`].
///
/// The driver only uses the `block_info.number` field from the reorg event
/// (for logging), so the other fields can be zeroed.
struct DummyL2Info;

impl DummyL2Info {
    fn at_number(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo::new(B256::ZERO, number, B256::ZERO, 0),
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        }
    }
}

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
    batcher.signal_reorg(DummyL2Info::at_number(7)).await;

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
    batcher.signal_reorg(DummyL2Info::at_number(5)).await;

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
// B. Gap-filling with safe head tracking (production-like path)
// ---------------------------------------------------------------------------

/// Same gap-filling scenario as above, but with a [`safe_head_rx`] watch
/// channel wired into the [`BatchDriver`]. This exercises the production
/// code path where `catchup_from = safe_head + 1` is computed from the
/// live safe-head feed.
///
/// The safe head watch also triggers [`prune_safe`] inside the encoder,
/// removing blocks that are confirmed safe and freeing encoder resources.
///
/// [`safe_head_rx`]: Batcher::with_safe_head_rx
/// [`prune_safe`]: base_batcher_encoder::BatchPipeline::prune_safe
/// [`BatchDriver`]: base_batcher_core::BatchDriver
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

    // Wire a safe-head watch channel into the batcher.
    let (safe_head_tx, safe_head_rx) = tokio::sync::watch::channel(0u64);
    let mut batcher = Batcher::with_safe_head_rx(
        ActionL2Source::new(),
        &h.rollup_config,
        batcher_cfg.clone(),
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
    safe_head_tx.send(5).expect("watch channel open");
    // Yield to let the driver process the safe-head update (prune_safe).
    tokio::task::yield_now().await;

    // ----- Phase 2: repoint to node B, post gap blocks 8-10 -----
    batcher.signal_reorg(DummyL2Info::at_number(7)).await;

    for block in &blocks[7..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(node.l2_safe_number(), 5, "Phase 2: safe head must stay at 5");
    assert_eq!(derived, 0, "Phase 2: no blocks derived (gap)");

    // ----- Phase 3: repoint back, fill the gap from safe_head + 1 = 6 -----
    // The batcher's safe_head_rx still reads 5, so the driver's
    // catchup_from = 5 + 1 = 6 — exactly the gap start.
    batcher.signal_reorg(DummyL2Info::at_number(5)).await;

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
// B2. Gap-filling triggered purely by a safe-head regression (L1 reorg model)
// ---------------------------------------------------------------------------

/// End-to-end coverage of the safe-head regression recovery path. A regression
/// on the watch channel, and nothing else, must let the batcher fill a gap and
/// let the verifier derive through it. There is no [`signal_reorg`] in the
/// recovery phase, unlike [`batcher_gap_fill_with_safe_head_tracking`].
///
/// This exercises the production wiring for the permanent-gap fix. After an L1
/// reorg drops batches that had advanced the safe head, the rollup node
/// re-derives a lower safe head, which arrives here as a watch-channel
/// regression. The driver must react to that regression alone by resetting the
/// encoder and restarting catchup from `safe_head + 1`, so the re-posted blocks
/// fill the gap.
///
/// Note on discrimination. This test drives the full wired path but is not by
/// itself a fail-without-fix guarantee, because the action harness prunes
/// fully-confirmed blocks from the encoder buffer (see
/// `BatchEncoder::confirm`), which empties it and sidesteps the parent-hash
/// guard on re-post. The exact regression behavior — reset plus
/// `reset_catchup(safe_head + 1)`, and never pruning on a regression — is pinned
/// by the discriminating unit tests `safe_head_regression_resets_and_catches_up`
/// and `safe_head_advance_prunes_without_reset` in `base-batcher-core`, and by
/// `poll_propagates_regression` in `base-batcher-service`.
///
/// Scenario:
///
/// 1. **Phase 1** — Batcher follows a divergent node (safe head 7 via
///    [`signal_reorg`]) and posts blocks 8-10. They land on L1 but are not
///    derivable because blocks 6-7 are missing, so the verifier stays at safe
///    head 5.
///
/// 2. **Phase 2** — The batcher's safe head latches at 7, the divergent node's
///    head. This is the bug precondition: a stale safe head sitting above the
///    true one.
///
/// 3. **Phase 3** — The L1 reorg drops those batches and the rollup node
///    re-derives the true safe head of 5. It arrives as a watch-channel
///    **regression** from 7 down to 5, with no [`signal_reorg`]. The driver must
///    reset the encoder and set `catchup_from = 6` on its own, so the re-posted
///    blocks 6-10 fill the gap and the verifier derives through to 10.
///
/// [`signal_reorg`]: Batcher::signal_reorg
/// [`batcher_gap_fill_with_safe_head_tracking`]: super::batcher_gap_fill_with_safe_head_tracking
#[tokio::test]
async fn batcher_gap_fill_on_safe_head_regression() {
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

    // Wire a safe-head watch channel into the batcher, seeded at 0.
    let (safe_head_tx, safe_head_rx) = tokio::sync::watch::channel(0u64);
    let mut batcher = Batcher::with_safe_head_rx(
        ActionL2Source::new(),
        &h.rollup_config,
        batcher_cfg.clone(),
        safe_head_rx,
    );

    // Establish the initial safe head at 5 by posting blocks 1-5.
    for block in &blocks[..5] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "setup: expected 5 L2 blocks derived");
    assert_eq!(node.l2_safe_number(), 5, "setup: safe head must be 5");
    safe_head_tx.send(5).expect("watch channel open");
    tokio::task::yield_now().await;

    // ----- Phase 1: follow a divergent node, post gap blocks 8-10 -----
    // signal_reorg models the batcher briefly following a divergent chain
    // (node B, safe head 7). Blocks 8-10 land on L1 but are not derivable, and
    // remain buffered in the encoder with the tip at block 10.
    batcher.signal_reorg(DummyL2Info::at_number(7)).await;
    for block in &blocks[7..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(node.l2_safe_number(), 5, "Phase 1: safe head must stay at 5 (gap 6-7 missing)");
    assert_eq!(derived, 0, "Phase 1: no blocks derived");

    // ----- Phase 2: latch the batcher safe head at the divergent head 7 -----
    // 7 is above blocks 8-10, so this does NOT prune them from the encoder.
    // They stay buffered with the tip at 10, the stale-forward precondition.
    safe_head_tx.send(7).expect("watch channel open");
    tokio::task::yield_now().await;

    // ----- Phase 3: the L1 reorg drops the batches; safe head regresses to 5 -----
    // No signal_reorg. The regression alone must reset the encoder and set
    // catchup_from = 6. Yield enough for the driver to process the reset.
    safe_head_tx.send(5).expect("watch channel open");
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    // Post blocks 6-10 from the true safe head to fill the gap.
    for block in &blocks[5..10] {
        batcher.push_block(block.clone());
    }
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 3: expected 5 L2 blocks derived (6-10)");
    assert_eq!(
        node.l2_safe_number(),
        10,
        "Phase 3: safe head must reach 10 after the regression-triggered gap fill"
    );
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
