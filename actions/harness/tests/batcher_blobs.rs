//! Action tests for blob DA submission and mixed calldata/blob derivation.

use std::sync::Arc;

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{BatchEncoder, BatchPipeline, DaType, EncoderConfig};

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

    // One block per L1 inclusion block.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 1..=3u64 {
        batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 3, "blob DA should derive 3 L2 blocks");
    assert_eq!(node.l2_safe_number(), 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Multi-blob packing (many frames → many blob sidecars in one L1 block)
// ---------------------------------------------------------------------------

/// Force channel fragmentation via a tiny `max_frame_size`, then verify that
/// all resulting frames are packed into a single blob sidecar in one L1 block
/// and that the derivation pipeline can reconstruct the L2 block from it.
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
    let block = sequencer.build_next_block_with_single_transaction().await;

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;

    // With frame packing, all frames from the fragmented channel are packed
    // into a single blob payload in one L1 transaction — exactly one blob sidecar.
    assert_eq!(
        h.l1.tip().blob_sidecars.len(),
        1,
        "expected all frames packed into one blob sidecar, got {}",
        h.l1.tip().blob_sidecars.len()
    );

    let (mut node, _chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected 1 L2 block derived from packed multi-frame blob");
    assert_eq!(node.l2_safe_number(), 1, "safe head should reach L2 block 1");
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

    // One block per L1 inclusion block.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 1..=3u64 {
        batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 3, "calldata DA should derive 3 L2 blocks");
    assert_eq!(node.l2_safe_number(), 3, "safe head should reach L2 block 3");
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
    let mut calldata_batcher =
        Batcher::new(ActionL2Source::new(), &h.rollup_config, calldata_cfg.clone());
    for _ in 1..=3u64 {
        calldata_batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        calldata_batcher.advance(&mut h.l1).await;
    }

    // Blocks 4-6: submit as blobs.
    let mut blob_batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, blob_cfg.clone());
    for _ in 4..=6u64 {
        blob_batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        blob_batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let mut total_derived = 0;
    for _ in 1..=6u64 {
        total_derived += node.run_until_idle().await;
    }

    assert_eq!(total_derived, 6, "expected 6 L2 blocks derived (3 calldata + 3 blob)");
    assert_eq!(node.l2_safe_number(), 6, "safe head should reach L2 block 6");
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
    let block = sequencer.build_next_block_with_single_transaction().await;

    // Encode the L2 block into multiple frames (tiny max_frame_size).
    // Use BatchEncoder directly so we can control exactly which frames go into
    // which L1 block — blob packing in the Batcher actor packs all frames into
    // one blob, which would make partial submission impossible.
    let mut encoder =
        BatchEncoder::new(Arc::new(h.rollup_config.clone()), batcher_cfg.encoder.clone());
    encoder.add_block(block.clone()).expect("add_block should succeed");
    let frames = encoder.encode_and_drain().expect("encode_and_drain should succeed");
    let frame_count = frames.len();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames"
    );

    // Submit ONLY frame 0 as a blob sidecar in L1 block 1.
    h.l1.submit_blob_frames(&frames[..1]);

    let (mut node, chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.l1.mine_block();
    chain.push(h.l1.tip().clone()); // L1 block 1: blob with frame 0 only

    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 0, "incomplete blob channel should not advance safe head");

    // Mine channel_timeout + 1 = 3 empty L1 blocks to expire the channel.
    for _ in 0..3 {
        h.mine_and_push(&chain);
    }
    for _ in 2..=4 {
        node.run_until_idle().await;
    }

    // Submit remaining frames as blobs — channel already timed out; silently dropped.
    h.l1.submit_blob_frames(&frames[1..]);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone()); // L1 block 5: late blob frames

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "late blob frames after channel timeout must be silently dropped");

    // Recovery: resubmit all frames as blobs in a fresh channel (new channel ID).
    let mut encoder2 =
        BatchEncoder::new(Arc::new(h.rollup_config.clone()), batcher_cfg.encoder.clone());
    encoder2.add_block(block).expect("add_block should succeed");
    let fresh_frames = encoder2.encode_and_drain().expect("encode_and_drain should succeed");
    h.l1.submit_blob_frames(&fresh_frames);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone()); // L1 block 6: fresh blob channel with all frames

    let recovered = node.run_until_idle().await;

    assert_eq!(recovered, 1, "resubmitted blob channel should derive L2 block 1");
    assert_eq!(node.l2_safe_number(), 1, "safe head should recover to 1");
}
