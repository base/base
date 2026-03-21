//! Action tests for channel timeout and interleaving scenarios.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

// ---------------------------------------------------------------------------
// A. Channel timeout — first frame's inclusion span exceeds channel_timeout
// ---------------------------------------------------------------------------

/// When a channel's frames are spread across L1 blocks separated by more than
/// `channel_timeout` blocks, the derivation pipeline discards the entire
/// channel. The batcher must detect this and resubmit the affected L2 blocks
/// in a new channel.
#[tokio::test]
async fn channel_timeout_triggers_channel_invalidation() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_channel_timeout(2).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction();

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode block via Batcher — produces multiple frames with max_frame_size=80.
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames",
    );

    // L1 block 1: submit only frame 0.
    batcher.stage_n_frames(&mut h.l1, 1);
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

    node.initialize().await;
    node.act_l1_head_signal(h.l1.block_info_at(1)).await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 0, "incomplete channel should not advance safe head");

    // Mine `channel_timeout + 1 = 3` empty L1 blocks to expire the channel.
    for _ in 0..3 {
        h.mine_and_push(&chain);
    }

    for i in 2..=4 {
        node.act_l1_head_signal(h.l1.block_info_at(i)).await;
        node.run_until_idle().await;
    }

    // Submit the remaining frames — they should be silently ignored (channel timed out).
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    let block_5_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_5_num).await;

    node.act_l1_head_signal(h.l1.block_info_at(5)).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "late frames after channel timeout must be ignored");

    // Recovery: new Batcher (new BatchEncoder = new channel ID) with all frames in one L1 block.
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    let mut batcher2 = Batcher::new(source2, &h.rollup_config, batcher_cfg.clone());
    batcher2.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.act_l1_head_signal(h.l1.block_info_at(6)).await;
    let recovered = node.run_until_idle().await;

    assert_eq!(recovered, 1, "resubmitted channel should derive L2 block 1");
    assert_eq!(node.l2_safe_number(), 1, "safe head should recover to 1");
}

// ---------------------------------------------------------------------------
// B. Channel timeout with recovery
// ---------------------------------------------------------------------------

/// After a channel times out, the batcher creates a fresh channel containing
/// the same L2 blocks and submits it within the timeout window.
#[tokio::test]
async fn channel_timeout_recovery_resubmits_successfully() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_channel_timeout(2).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction();

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode the block — will produce multiple frames with max_frame_size=80.
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames",
    );

    // L1 block 1: submit only frame 0 — channel stays incomplete.
    batcher.stage_n_frames(&mut h.l1, 1);
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

    node.initialize().await;

    // Mine channel_timeout + 1 = 3 empty blocks to expire the channel.
    for _ in 0..3 {
        h.mine_and_push(&chain);
    }

    for i in 1..=h.l1.latest_number() {
        node.act_l1_head_signal(h.l1.block_info_at(i)).await;
        node.run_until_idle().await;
    }

    assert_eq!(
        node.l2_safe_number(),
        0,
        "channel should have timed out; safe head must remain at genesis"
    );

    // Recovery: new Batcher (new channel ID) submits all frames in one L1 block.
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    let mut batcher2 = Batcher::new(source2, &h.rollup_config, batcher_cfg.clone());
    batcher2.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.act_l1_head_signal(h.l1.block_info_at(h.l1.latest_number())).await;
    let recovered = node.run_until_idle().await;

    assert_eq!(recovered, 1, "recovery channel should derive L2 block 1");
    assert_eq!(node.l2_safe_number(), 1, "safe head should recover to 1");
}

// ---------------------------------------------------------------------------
// C. Channel interleaving — frames from two channels interleaved in L1
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
    let block_a = sequencer.build_next_block_with_single_transaction();
    let block_b = sequencer.build_next_block_with_single_transaction();

    // Batcher A: block 1 in its own channel (distinct random channel ID).
    let mut source_a = ActionL2Source::new();
    source_a.push(block_a);
    let mut batcher_a = Batcher::new(source_a, &h.rollup_config, batcher_cfg.clone());
    batcher_a.encode_only().await;

    // Batcher B: block 2 in its own channel (distinct random channel ID).
    let mut source_b = ActionL2Source::new();
    source_b.push(block_b);
    let mut batcher_b = Batcher::new(source_b, &h.rollup_config, batcher_cfg.clone());
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
    let block_num = h.l1.mine_block().number();
    batcher_a.confirm_staged(block_num).await;
    batcher_b.confirm_staged(block_num).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    node.act_l1_head_signal(h.l1.block_info_at(1)).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "expected 2 L2 blocks derived from interleaved channels");
    assert_eq!(node.l2_safe_number(), 2);
}

// ---------------------------------------------------------------------------
// D. Multi-block channel — frames split across consecutive L1 blocks
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
    let block = sequencer.build_next_block_with_single_transaction();

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode into multiple frames.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "need at least 2 frames for this test; got {frame_count} (increase payload or decrease max_frame_size)",
    );

    // L1 block 1: frame 0 only.
    batcher.stage_n_frames(&mut h.l1, 1);
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

    node.initialize().await;
    node.act_l1_head_signal(h.l1.block_info_at(1)).await;
    node.run_until_idle().await;

    assert_eq!(
        node.l2_safe_number(),
        0,
        "channel incomplete after block 1; safe head must stay at genesis"
    );

    // L1 block 2: remaining frames (well within channel_timeout).
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    let block_2_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_2_num).await;

    node.act_l1_head_signal(h.l1.block_info_at(2)).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "multi-block channel must yield 1 L2 block");
    assert_eq!(node.l2_safe_number(), 1, "safe head must advance to 1");
}

// ---------------------------------------------------------------------------
// E. Multi-frame channel with an empty L1 gap between submissions
// ---------------------------------------------------------------------------

/// Frames from a single channel are submitted to L1 in two separate L1 blocks
/// with an **empty L1 block** between them. The derivation pipeline must
/// correctly reassemble the channel across the gap.
///
/// Note: `encode_only()` sends a `Flush` event that closes the channel
/// immediately, so all frames are in the pending queue before any L1 head
/// events arrive. This means this test exercises the multi-frame split
/// submission scenario (frame 0 in block 1, empty block 2, rest in block 3),
/// not duration-based channel closure — that would require the channel to
/// remain open while L1 blocks are mined.
#[tokio::test]
async fn multi_frame_channel_with_empty_l1_gap_derives_correctly() {
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
    let block = sequencer.build_next_block_with_single_transaction();

    // Create node before any mining so all future blocks are pushed to chain.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode block — produces multiple frames with max_frame_size=80.
    // The Flush from encode_only() closes the channel; frames become pending.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames",
    );

    // L1 block 1: submit only frame 0.
    batcher.stage_n_frames(&mut h.l1, 1);
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

    node.initialize().await;
    node.act_l1_head_signal(h.l1.block_info_at(1)).await;
    node.run_until_idle().await;

    assert_eq!(
        node.l2_safe_number(),
        0,
        "incomplete channel after block 1; safe head must stay at genesis"
    );

    // Mine an empty L1 block 2. The channel was already closed by encode_only()
    // (which sent Flush), so no staged items are confirmed here. The call to
    // confirm_staged is used solely to advance the driver's L1 head to block 2
    // via L1HeadEvent::NewHead — confirm_all fires zero receipts and just sends
    // the head event. The remaining frames are already in `pending`.
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(h.l1.latest_number()).await;

    // L1 block 3: submit the remaining frames.
    let remaining = batcher.pending_count();
    batcher.stage_n_frames(&mut h.l1, remaining);
    let block_3_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_3_num).await;

    // Signal node for all L1 blocks. Track the total L2 blocks derived
    // to confirm exactly one block was produced across the 3-block span.
    let mut total_derived = 0usize;
    for i in 2..=h.l1.latest_number() {
        node.act_l1_head_signal(h.l1.block_info_at(i)).await;
        total_derived += node.run_until_idle().await;
    }

    assert_eq!(total_derived, 1, "exactly one L2 block must be derived across the 3-block span");
    assert_eq!(
        node.l2_safe_number(),
        1,
        "frames split across 3 L1 blocks (with an empty intermediate block) must derive L2 block 1"
    );
}
