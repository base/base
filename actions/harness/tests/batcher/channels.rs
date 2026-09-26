//! Action tests for channel timeout and interleaving scenarios.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// The derivation channel timeout, in L1 blocks, of the channel-timeout test.
const CHANNEL_TIMEOUT: u64 = 2;

// ---------------------------------------------------------------------------
// A. Channel timeout — first frame's inclusion span exceeds channel_timeout
// ---------------------------------------------------------------------------

/// A channel whose frames land more than `channel_timeout` L1 blocks apart is discarded by
/// derivation, late frames included. How the batcher recovers from that is in `recovery.rs`,
/// and the Granite value of the timeout in `upgrade_transitions.rs`.
#[tokio::test]
async fn late_frames_of_a_timed_out_channel_are_ignored() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            // Below `CHANNEL_TIMEOUT`, as `validate_for_rollup_config` requires.
            max_channel_duration: 1,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_channel_timeout(CHANNEL_TIMEOUT)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode block via Batcher — produces multiple frames with max_frame_size=80.
    let batcher = Batcher::new(ActionL2Source::from_blocks([block]), &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames",
    );

    // L1 block 1: submit only frame 0.
    batcher.stage_n_frames(&mut h.l1, 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 0, "incomplete channel should not advance safe head");

    // Mine `CHANNEL_TIMEOUT + 1` empty L1 blocks to expire the channel.
    for _ in 0..=CHANNEL_TIMEOUT {
        h.mine_and_push(&chain);
        node.run_until_idle().await;
    }

    // Submit the remaining frames — they should be silently ignored (channel timed out).
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "late frames after channel timeout must be ignored");
    assert_eq!(node.l2_safe_number(), 0);
}

// ---------------------------------------------------------------------------
// B. Channel interleaving — frames from two channels interleaved in L1
// ---------------------------------------------------------------------------

/// Frames from two different channels are submitted to L1 in interleaved
/// order (A0, B0, A1, B1). The derivation pipeline's channel bank must
/// correctly track both channels simultaneously and reassemble them
/// independently.
#[tokio::test]
async fn interleaved_channels_correctly_reassembled() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block_a = sequencer.build_next_block_with_single_transaction().await;
    let block_b = sequencer.build_next_block_with_single_transaction().await;

    // Batcher A: block 1 in its own channel (distinct random channel ID).
    let mut source_a = ActionL2Source::new();
    source_a.push(block_a);
    let batcher_a = Batcher::new(source_a, &h.rollup_config, batcher_cfg.clone());
    batcher_a.encode_only().await;

    // Batcher B: block 2 in its own channel (distinct random channel ID).
    let mut source_b = ActionL2Source::new();
    source_b.push(block_b);
    let batcher_b = Batcher::new(source_b, &h.rollup_config, batcher_cfg.clone());
    batcher_b.encode_only().await;

    let n_a = batcher_a.pending_count();
    let n_b = batcher_b.pending_count();
    assert!(n_a >= 2, "channel A must produce 2+ frames with max_frame_size=80, got {n_a}");
    assert!(n_b >= 2, "channel B must produce 2+ frames with max_frame_size=80, got {n_b}");

    // Interleave frames: A0, B0, A1, B1, ...
    for i in 0..n_a.max(n_b) {
        if i < n_a {
            batcher_a.stage_n_frames(&mut h.l1, 1);
        }
        if i < n_b {
            batcher_b.stage_n_frames(&mut h.l1, 1);
        }
    }

    // Mine one L1 block containing all interleaved frames.
    h.l1.mine_block();
    batcher_a.observe_l1_block(h.l1.tip()).await;
    batcher_b.observe_l1_block(h.l1.tip()).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "expected 2 L2 blocks derived from interleaved channels");
    assert_eq!(node.l2_safe_number(), 2);
}

// ---------------------------------------------------------------------------
// C. Multi-block channel — frames split across consecutive L1 blocks
// ---------------------------------------------------------------------------

/// A single channel whose frames are spread across two consecutive L1 blocks
/// is correctly reassembled by the derivation pipeline.
#[tokio::test]
async fn multi_block_channel_assembles_across_l1_blocks() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode into multiple frames.
    let mut source = ActionL2Source::new();
    source.push(block);
    let batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "need at least 2 frames for this test; got {frame_count} (increase payload or decrease max_frame_size)",
    );

    // L1 block 1: frame 0 only.
    batcher.stage_n_frames(&mut h.l1, 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(
        node.l2_safe_number(),
        0,
        "channel incomplete after block 1; safe head must stay at genesis"
    );

    // L1 block 2: remaining frames (well within channel_timeout).
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "multi-block channel must yield 1 L2 block");
    assert_eq!(node.l2_safe_number(), 1, "safe head must advance to 1");
}
