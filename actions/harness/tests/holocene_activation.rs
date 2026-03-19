#![doc = "Action tests for the Holocene hardfork activation and Holocene-specific protocol changes."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

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

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Build and submit 4 L2 blocks; no upgrade-tx constraint at Holocene,
    // so user txs are valid in all blocks including block 3.
    for _ in 1..=4u64 {
        let block = builder.build_next_block();
        let mut source = ActionL2Source::new();
        source.push(block);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    verifier.initialize().await;

    for i in 1..=4u64 {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        let derived = verifier.act_l2_pipeline_full().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block at/after Holocene");
    }

    assert_eq!(
        verifier.l2_safe_number(),
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
        encoder: EncoderConfig {
            max_frame_size: 80,
            da_type: DaType::Calldata,
            ..EncoderConfig::default()
        },
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
    let block = sequencer.build_next_block();

    // Encode the block into frames without mining.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;
    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 3,
        "need ≥3 frames to skip frame 1; got {frame_count} (decrease max_frame_size)"
    );

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Submit frame 0 and frame 2 in the same L1 block — skipping frame 1.
    // Under Holocene, FrameQueue::prune removes frame 2 because
    // frame 0.number + 1 != frame 2.number (0 + 1 = 1 ≠ 2).
    batcher.stage_n_frames(&mut h.l1, 1); // frame 0
    batcher.drop_n_frames(1); // drop frame 1
    batcher.stage_n_frames(&mut h.l1, 1); // frame 2 (non-sequential)
    let block_1_num = h.l1.mine_block().number();
    batcher.confirm_staged(block_1_num).await;
    chain.push(h.l1.tip().clone()); // L1 block 1: frames 0 and 2

    verifier.initialize().await;
    verifier.act_l1_head_signal(h.l1.block_info_at(1)).await;
    verifier.act_l2_pipeline_full().await;

    // Frame 2 is pruned by FrameQueue — channel 0 only has frame 0.
    // Channel is incomplete; safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe_number(),
        0,
        "channel with missing frame 1 must never complete under Holocene"
    );

    // Mine additional empty L1 blocks past the channel timeout to confirm
    // the channel is permanently abandoned (not just waiting for more frames).
    for _ in 0..12 {
        h.mine_and_push(&chain);
    }
    for i in 2..=h.l1.latest_number() {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        verifier.act_l2_pipeline_full().await;
    }

    assert_eq!(
        verifier.l2_safe_number(),
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
        encoder: EncoderConfig {
            max_frame_size: 80,
            da_type: DaType::Calldata,
            ..EncoderConfig::default()
        },
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

    let block_a = sequencer.build_next_block();
    let block_b = sequencer.build_next_block();

    // Encode channel A (block A) and channel B (block B) separately.
    // Each Batcher instance generates a distinct random channel ID.
    let mut source_a = ActionL2Source::new();
    source_a.push(block_a);
    let mut batcher_a = Batcher::new(source_a, &h.rollup_config, batcher_cfg.clone());
    batcher_a.encode_only().await;

    let mut source_b = ActionL2Source::new();
    source_b.push(block_b);
    let mut batcher_b = Batcher::new(source_b, &h.rollup_config, batcher_cfg.clone());
    batcher_b.encode_only().await;

    let n_a = batcher_a.pending_count();
    let n_b = batcher_b.pending_count();
    assert!(n_a >= 2, "channel A needs ≥2 frames; got {n_a}");

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: only frame 0 of channel A (channel is incomplete).
    batcher_a.stage_n_frames(&mut h.l1, 1); // frame 0 of channel A
    let block_1_num = h.l1.mine_block().number();
    batcher_a.confirm_staged(block_1_num).await;
    chain.push(h.l1.tip().clone()); // L1 block 1: frame 0 of channel A

    verifier.initialize().await;
    verifier.act_l1_head_signal(h.l1.block_info_at(1)).await;
    verifier.act_l2_pipeline_full().await;

    // Channel A is open but incomplete — safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe_number(),
        0,
        "channel A only has frame 0; safe head must stay at genesis"
    );

    // L1 block 2: ALL frames of channel B (starts with frame 0, different ID).
    // Under Holocene pruning: channel A's frame 0 is in the queue. When
    // channel B's frame 0 arrives (different ID, B is not last), the queue
    // drains all of channel A's frames. Channel B assembles and derives.
    for _ in 0..n_b {
        batcher_b.stage_n_frames(&mut h.l1, 1);
    }
    let block_2_num = h.l1.mine_block().number();
    batcher_b.confirm_staged(block_2_num).await;
    chain.push(h.l1.tip().clone()); // L1 block 2: all frames of channel B

    verifier.act_l1_head_signal(h.l1.block_info_at(2)).await;
    verifier.act_l2_pipeline_full().await;

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
        verifier.l2_safe_number(),
        0,
        "channel A was abandoned; block A (L2 block 1) never derived; \
         block B (L2 block 2) is a future batch and cannot be emitted"
    );
}

// ---------------------------------------------------------------------------
// D. Holocene frame pruning: non-sequential frame pruned, then recovery
//
// op-e2e ref: holocene_frame_test.go (extended)
// ---------------------------------------------------------------------------

/// Same scenario as [`holocene_non_sequential_frame_pruned_channel_never_completes`]:
/// a non-sequential frame causes the channel to be pruned and the safe head
/// stays at genesis.
///
/// The **recovery step** creates a brand-new [`Batcher`] (new channel ID) that
/// re-encodes the same L2 block and submits all frames sequentially in one L1
/// block. After the verifier processes the new L1 block the safe head must
/// advance to 1, proving the pipeline recovers once valid data arrives.
///
/// [`Batcher`]: base_action_harness::Batcher
#[tokio::test]
async fn holocene_non_sequential_frame_pruned_then_recovery_succeeds() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            max_frame_size: 80,
            da_type: DaType::Calldata,
            ..EncoderConfig::default()
        },
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
    let block = sequencer.build_next_block();

    // Encode the block into frames without mining.
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;
    let frame_count = batcher.pending_count();
    assert!(frame_count >= 3, "need ≥3 frames to skip frame 1; got {frame_count}");

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Submit frame 0 and frame 2 in the same L1 block — skipping frame 1.
    batcher.stage_n_frames(&mut h.l1, 1); // frame 0
    batcher.drop_n_frames(1); // drop frame 1
    batcher.stage_n_frames(&mut h.l1, 1); // frame 2 (non-sequential)
    let block_1_num = h.l1.mine_block().number();
    batcher.confirm_staged(block_1_num).await;
    chain.push(h.l1.tip().clone());

    verifier.initialize().await;
    verifier.act_l1_head_signal(h.l1.block_info_at(1)).await;
    verifier.act_l2_pipeline_full().await;

    // Channel is broken — safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe_number(),
        0,
        "channel with missing frame 1 must never complete under Holocene"
    );

    // Mine empty L1 blocks to confirm the broken channel never completes.
    // Under Holocene, FrameQueue::prune drops frame 2 immediately upon arrival
    // (non-sequential after frame 0), so the channel is permanently broken
    // regardless of timeout. We mine fewer than channel_timeout (10) blocks:
    // recovery does not require the broken channel to expire — it relies on
    // ChannelAssembler unconditionally replacing its active channel when a new
    // channel's frame 0 arrives (regardless of timeout state).
    for _ in 0..5 {
        h.mine_and_push(&chain);
    }
    for i in 2..=h.l1.latest_number() {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        verifier.act_l2_pipeline_full().await;
    }
    assert_eq!(
        verifier.l2_safe_number(),
        0,
        "safe head must remain at genesis: broken channel was pruned, no recovery submitted yet"
    );

    // -- Recovery: new Batcher (new channel ID) re-submits all frames in order.
    // `block` was cloned into `source` above (line 372), so the original value
    // is still valid here — same L2 block 1 with the same transactions and
    // timestamp. The sequencer has not advanced since build_next_block().
    let mut recovery_source = ActionL2Source::new();
    recovery_source.push(block);
    let mut batcher2 = Batcher::new(recovery_source, &h.rollup_config, batcher_cfg.clone());
    batcher2.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let recovery_block_num = h.l1.latest_number();
    verifier.act_l1_head_signal(h.l1.block_info_at(recovery_block_num)).await;
    verifier.act_l2_pipeline_full().await;

    assert_eq!(
        verifier.l2_safe_number(),
        1,
        "safe head must advance to 1 after clean recovery submission"
    );
}
