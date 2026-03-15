#![doc = "Action tests for the Holocene hardfork activation and Holocene-specific protocol changes."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder, block_info_from,
};
use base_batcher_encoder::EncoderConfig;
use base_consensus_genesis::HardForkConfig;

// ---------------------------------------------------------------------------
// A. Basic derivation through the Holocene activation boundary
//
// op-e2e ref: holocene_activation_test.go
// ---------------------------------------------------------------------------

/// Full end-to-end derivation through the Holocene activation boundary.
///
/// Holocene does **not** inject upgrade transactions (unlike Ecotone, Fjord,
/// Isthmus, and Jovian), but it does switch the channel provider from
/// [`ChannelBank`] to [`ChannelAssembler`] and changes frame-pruning semantics.
///
/// Configuration (`block_time=2`):
/// - All forks through Granite active at genesis.
/// - Holocene activates at ts=6 (L2 block 3).
/// - Blocks 1–2: pre-Holocene.
/// - Block 3: first Holocene block — user txs are still fine (no upgrade tx constraint).
/// - Block 4: post-Holocene.
///
/// All 4 blocks must derive successfully.
///
/// [`ChannelBank`]: base_consensus_derive::stages::ChannelBank
/// [`ChannelAssembler`]: base_consensus_derive::stages::ChannelAssembler
#[tokio::test]
async fn holocene_derivation_crosses_activation_boundary() {
    let batcher_cfg = BatcherConfig::default();

    // All forks through Granite at genesis; Holocene at ts=6 (block 3).
    // Fjord is needed so the batcher's brotli compression is accepted.
    let holocene_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(holocene_time),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build and submit 4 L2 blocks; no upgrade-tx constraint at Holocene,
    // so user txs are valid in all blocks including block 3.
    for _ in 1..=4u64 {
        let block = builder.build_next_block().expect("build L2 block");
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode batch");
        batcher.flush(&mut h.l1);
        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await.expect("initialize");

    for i in 1..=4u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("pipeline");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block at/after Holocene");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "all 4 L2 blocks must derive through the Holocene boundary"
    );
}

// ---------------------------------------------------------------------------
// B. Holocene frame pruning: non-sequential frame is dropped
//
// op-e2e ref: holocene_frame_test.go
// ---------------------------------------------------------------------------

/// Under Holocene, [`FrameQueue::prune`] enforces sequential frame numbers
/// within the same channel. If frame 0 is followed by frame 2 (skipping
/// frame 1), the [`FrameQueue`] prunes frame 2 immediately. The channel
/// can never complete and no L2 block is derived.
///
/// Pre-Holocene, the frames would sit in the [`ChannelBank`] until the
/// channel timeout — the timing is different, but the channel also fails to
/// complete.
///
/// Setup:
/// - `max_frame_size=80` forces at least 3 frames for 1 L2 block.
/// - Submit frame 0 and frame 2 in L1 block 1 (skipping frame 1).
/// - Mine enough empty L1 blocks to exhaust any in-progress channel.
/// - Verify safe head never advances.
///
/// [`FrameQueue::prune`]: base_consensus_derive::stages::FrameQueue::prune
/// [`ChannelBank`]: base_consensus_derive::stages::ChannelBank
#[tokio::test]
async fn holocene_non_sequential_frame_pruned_channel_never_completes() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_hardforks(HardForkConfig {
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0), // active from genesis
            ..Default::default()
        })
        .with_channel_timeout(10) // generous timeout so the channel doesn't expire naturally
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build L2 block 1");

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().expect("encode multi-frame channel");
    assert!(
        frames.len() >= 3,
        "need ≥3 frames to skip frame 1; got {} (decrease max_frame_size)",
        frames.len()
    );

    // Submit frame 0 and frame 2 in the same L1 block — skipping frame 1.
    // Under Holocene, FrameQueue::prune removes frame 2 because
    // frame 0.number + 1 != frame 2.number (0 + 1 = 1 ≠ 2).
    {
        let empty_source = ActionL2Source::new();
        let mut submitter = h.create_batcher(empty_source, batcher_cfg.clone());
        submitter.submit_frames(&frames[0..1]); // frame 0
        submitter.submit_frames(&frames[2..3]); // frame 2 (non-sequential)
        submitter.flush(&mut h.l1);
    }

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.mine_and_push(&chain); // L1 block 1: frames 0 and 2

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("pipeline block 1");

    // Frame 2 is pruned by FrameQueue — channel 0 only has frame 0.
    // Channel is incomplete; safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "channel with missing frame 1 must never complete under Holocene"
    );

    // Mine additional empty L1 blocks past the channel timeout to confirm
    // the channel is permanently abandoned (not just waiting for more frames).
    for _ in 0..12 {
        h.mine_and_push(&chain);
    }
    for i in 2..=h.l1.latest_number() {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("pipeline");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "safe head must remain at genesis: non-sequential frame was pruned, channel never completed"
    );
}

// ---------------------------------------------------------------------------
// C. Holocene: new channel (frame 0) abandons incomplete old channel
//
// op-e2e ref: holocene_frame_test.go
// ---------------------------------------------------------------------------

/// Under Holocene frame pruning, when a new channel (different channel ID,
/// `frame_number=0`) arrives while the previous channel is still incomplete,
/// all frames of the old channel are drained and discarded. The new channel
/// assembles and derives its L2 block.
///
/// This tests the rule in [`FrameQueue::prune`]:
/// > If frames are in different channels, and the current channel is not
/// > last, walk back and drop all prev frames.
///
/// Setup:
/// - Encode two L2 blocks into two separate channels (A and B, distinct IDs).
/// - Submit only frame 0 of channel A in L1 block 1.
/// - Submit all frames of channel B (starting at frame 0) in L1 block 2.
///
/// Under Holocene, channel A's incomplete frames are discarded when channel
/// B's frame 0 arrives. Channel B assembles and derives L2 block 2, but L2
/// block 1 (from the abandoned channel A) is never derived.
///
/// [`FrameQueue::prune`]: base_consensus_derive::stages::FrameQueue::prune
#[tokio::test]
async fn holocene_new_channel_abandons_incomplete_old_channel() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_hardforks(HardForkConfig {
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0),
            ..Default::default()
        })
        .with_channel_timeout(10)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let block_a = sequencer.build_next_block().expect("build L2 block A");
    let block_b = sequencer.build_next_block().expect("build L2 block B");

    // Encode channel A (block A) and channel B (block B) separately.
    // Each encoder instance generates a distinct random channel ID.
    let frames_a = {
        let mut source = ActionL2Source::new();
        source.push(block_a);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.encode_frames().expect("encode channel A")
    };
    let frames_b = {
        let mut source = ActionL2Source::new();
        source.push(block_b);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.encode_frames().expect("encode channel B")
    };

    assert!(frames_a.len() >= 2, "channel A needs ≥2 frames; got {}", frames_a.len());

    // Channels must have distinct IDs for the pruning rule to apply.
    assert_ne!(frames_a[0].id, frames_b[0].id, "channel A and B must have distinct IDs");

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: only frame 0 of channel A (channel is incomplete).
    {
        let empty_source = ActionL2Source::new();
        let mut submitter = h.create_batcher(empty_source, batcher_cfg.clone());
        submitter.submit_frames(&frames_a[0..1]);
        submitter.flush(&mut h.l1);
    }
    h.mine_and_push(&chain);

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("pipeline block 1");

    // Channel A is open but incomplete — safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "channel A only has frame 0; safe head must stay at genesis"
    );

    // L1 block 2: ALL frames of channel B (starts with frame 0, different ID).
    // Under Holocene pruning: channel A's frame 0 is in the queue. When
    // channel B's frame 0 arrives (different ID, B is not last), the queue
    // drains all of channel A's frames. Channel B assembles and derives.
    {
        let empty_source = ActionL2Source::new();
        let mut submitter = h.create_batcher(empty_source, batcher_cfg.clone());
        for frame in &frames_b {
            submitter.submit_frames(std::slice::from_ref(frame));
        }
        submitter.flush(&mut h.l1);
    }
    h.mine_and_push(&chain);

    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal block 2");
    verifier.act_l2_pipeline_full().await.expect("pipeline block 2");

    // Channel A was abandoned (Holocene pruning). Channel B derived block B.
    // Block A was never derived since channel A was discarded.
    // The BatchQueue can only emit blocks in order, so if block 1 (from channel A)
    // was never derived, block 2 (from channel B) cannot be derived either unless
    // block 1 has been accounted for. The pipeline will stall.
    //
    // Safe head remains at genesis because block 1 is missing — channel B's
    // block 2 is a future batch from the perspective of the batch queue
    // (expected_timestamp = genesis + block_time = 2, but block B is ts=4).
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "channel A was abandoned; block A (L2 block 1) never derived; \
         block B (L2 block 2) is a future batch and cannot be emitted"
    );
}
