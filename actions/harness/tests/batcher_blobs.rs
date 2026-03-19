#![doc = "Action tests for blob DA submission and mixed calldata/blob derivation."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};

// ---------------------------------------------------------------------------
// Blob DA end-to-end
// ---------------------------------------------------------------------------

/// Encode 3 L2 blocks as EIP-4844 blobs (one blob per L2 block, each in its
/// own L1 block) and verify that the blob verifier pipeline derives all three.
#[tokio::test]
async fn batcher_blob_da_end_to_end() {
    let batcher_cfg = BatcherConfig::default(); // DaType::Blob by default
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // One Batcher per L2 block so each lands in a separate L1 block.
    for _ in 1..=3u64 {
        let block = sequencer.build_next_block();
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await;
    }

    let (mut verifier, _chain) = h.create_blob_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await;

    for i in 1..=3u64 {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        let derived = verifier.act_l2_pipeline_full().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block via blob DA");
    }

    assert_eq!(verifier.l2_safe_number(), 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Multi-blob packing (many frames → many blob sidecars in one L1 block)
// ---------------------------------------------------------------------------

/// Force channel fragmentation via a tiny `max_frame_size`, then submit all
/// resulting frames as separate blob sidecars in a single L1 block.
#[tokio::test]
async fn batcher_multi_blob_packing() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block();

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;

    // With max_frame_size=80, the block must have been fragmented into multiple
    // blob sidecars in the mined L1 block.
    assert!(
        h.l1.tip().blob_sidecars.len() >= 2,
        "expected multiple blob sidecars with max_frame_size=80, got {}",
        h.l1.tip().blob_sidecars.len()
    );

    let (mut verifier, _chain) = h.create_blob_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await;

    verifier.act_l1_head_signal(h.l1.block_info_at(1)).await;
    let derived = verifier.act_l2_pipeline_full().await;

    assert_eq!(derived, 1, "expected 1 L2 block derived from multi-blob channel");
    assert_eq!(verifier.l2_safe_number(), 1, "safe head should reach L2 block 1");
}

// ---------------------------------------------------------------------------
// Calldata DA (explicit)
// ---------------------------------------------------------------------------

/// Encode 3 L2 blocks as calldata frames and verify the calldata verifier
/// pipeline derives all three.
#[tokio::test]
async fn batcher_calldata_da() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // One Batcher per L2 block so each lands in a separate L1 block.
    for _ in 1..=3u64 {
        let block = sequencer.build_next_block();
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await;
    }

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await;

    for i in 1..=3u64 {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        let derived = verifier.act_l2_pipeline_full().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block via calldata DA");
    }

    assert_eq!(verifier.l2_safe_number(), 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Mixed calldata + blob derivation
// ---------------------------------------------------------------------------

/// Submit 3 L2 blocks as calldata and 3 more as blobs, each in separate L1
/// blocks, then derive all 6 using the blob verifier pipeline.
#[tokio::test]
async fn batcher_da_switching() {
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&BatcherConfig::default()).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let calldata_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let blob_cfg = BatcherConfig::default(); // DaType::Blob by default

    // Blocks 1-3: submit as calldata.
    for _ in 1..=3u64 {
        let block = sequencer.build_next_block();
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = Batcher::new(source, &h.rollup_config, calldata_cfg.clone());
        batcher.advance(&mut h.l1).await;
    }

    // Blocks 4-6: submit as blobs.
    for _ in 4..=6u64 {
        let block = sequencer.build_next_block();
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = Batcher::new(source, &h.rollup_config, blob_cfg.clone());
        batcher.advance(&mut h.l1).await;
    }

    let (mut verifier, _chain) = h.create_blob_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await;

    let mut total_derived = 0;
    for i in 1..=6u64 {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        total_derived += verifier.act_l2_pipeline_full().await;
    }

    assert_eq!(total_derived, 6, "expected 6 L2 blocks derived (3 calldata + 3 blob)");
    assert_eq!(verifier.l2_safe_number(), 6, "safe head should reach L2 block 6");
}

// ---------------------------------------------------------------------------
// Blob DA channel timeout
// ---------------------------------------------------------------------------

/// A blob DA channel that is not completed within `channel_timeout` L1 blocks
/// is discarded by the pipeline. Late blob frames for the timed-out channel
/// are silently ignored; a fresh channel submitted inside the window recovers.
///
/// This is the blob-DA variant of
/// `channel_timeout_triggers_channel_invalidation` in `batcher_channels.rs`.
/// It uses `submit_blob_frames` instead of `submit_frames` throughout.
///
/// Setup:
/// - `max_frame_size = 80` to force a multi-frame channel.
/// - `channel_timeout = 2` (very tight: expires after 2 L1 blocks).
/// - Frame 0 submitted as a blob sidecar in L1 block 1.
/// - L1 blocks 2–4 are empty (`channel_timeout` + 1 = 3 blocks).
/// - Remaining frames arrive as blobs in L1 block 5 — channel already timed out.
/// - Recovery: all frames resubmitted in a fresh channel (L1 block 6).
///
/// The safe head must remain at 0 through L1 block 5, then advance to 1 after
/// the fresh blob channel is processed.
#[tokio::test]
async fn blob_da_channel_timeout() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_channel_timeout(2).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block();

    // Encode the L2 block into multiple frames (tiny max_frame_size).
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;
    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames"
    );

    // Submit ONLY frame 0 as a blob sidecar in L1 block 1.
    batcher.stage_n_frames(&mut h.l1, 1);

    let (mut verifier, chain) = h.create_blob_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let block_1_num = h.l1.mine_block().number();
    batcher.confirm_staged(block_1_num).await;
    chain.push(h.l1.tip().clone()); // L1 block 1: blob with frame 0 only

    verifier.initialize().await;
    verifier.act_l1_head_signal(h.l1.block_info_at(1)).await;
    verifier.act_l2_pipeline_full().await;

    assert_eq!(
        verifier.l2_safe_number(),
        0,
        "incomplete blob channel should not advance safe head"
    );

    // Mine channel_timeout + 1 = 3 empty L1 blocks to expire the channel.
    for _ in 0..3 {
        h.mine_and_push(&chain);
    }
    for i in 2..=4 {
        verifier.act_l1_head_signal(h.l1.block_info_at(i)).await;
        verifier.act_l2_pipeline_full().await;
    }

    // Submit remaining frames as blobs — channel already timed out; silently dropped.
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    let block_5_num = h.l1.mine_block().number();
    batcher.confirm_staged(block_5_num).await;
    chain.push(h.l1.tip().clone()); // L1 block 5: late blob frames

    verifier.act_l1_head_signal(h.l1.block_info_at(5)).await;
    let derived = verifier.act_l2_pipeline_full().await;
    assert_eq!(derived, 0, "late blob frames after channel timeout must be silently dropped");

    // Recovery: resubmit all frames as blobs in a fresh channel.
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    Batcher::new(source2, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone()); // L1 block 6: fresh blob channel with all frames

    verifier.act_l1_head_signal(h.l1.block_info_at(6)).await;
    let recovered = verifier.act_l2_pipeline_full().await;

    assert_eq!(recovered, 1, "resubmitted blob channel should derive L2 block 1");
    assert_eq!(verifier.l2_safe_number(), 1, "safe head should recover to 1");
}
