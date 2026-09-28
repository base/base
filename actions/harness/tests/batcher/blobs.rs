//! Action tests for blob DA submission and mixed calldata/blob derivation.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_blobs::BlobDecoder;
use base_protocol::Frame;

// ---------------------------------------------------------------------------
// Blob DA end-to-end
// ---------------------------------------------------------------------------

/// Encode 3 L2 blocks with EIP-4844 DA and verify that the blob verifier
/// pipeline derives all three.
#[tokio::test]
async fn batcher_blob_da_end_to_end() {
    let batcher_cfg = BatcherConfig::default(); // DaType::Blob by default
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // One block per L1 inclusion block.
    let batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
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
    assert_eq!(total_derived, 3, "blob DA should derive 3 L2 blocks");
    assert_eq!(node.l2_safe_number(), 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Multi-frame packing (many frames, one blob sidecar)
// ---------------------------------------------------------------------------

/// Frames much smaller than a blob share one blob sidecar, and derivation reads the block
/// from it.
#[tokio::test]
async fn small_frames_share_one_blob_and_derive() {
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
    let batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;

    let sidecars = &h.l1.tip().blob_sidecars;
    assert_eq!(sidecars.len(), 1, "fragmented frames should share one blob sidecar");
    let data = BlobDecoder::decode(&sidecars[0].1).expect("blob decodes");
    let frames = Frame::parse_frames(&data).expect("blob data parses");
    assert!(frames.len() >= 2, "the block must span several frames, got {}", frames.len());

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected 1 L2 block derived from packed multi-frame blob");
    assert_eq!(node.l2_safe_number(), 1, "safe head should reach L2 block 1");
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

    // Blocks 1-3: submit as calldata.
    let calldata_batcher =
        Batcher::new(ActionL2Source::new(), &h.rollup_config, calldata_cfg.clone());
    for _ in 1..=3u64 {
        calldata_batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        calldata_batcher.advance(&mut h.l1).await;
    }

    // Blocks 4-6: submit as blobs, from a second batcher that starts after block 3.
    let blob_cfg = BatcherConfig {
        initial_safe_head: Some(sequencer.head().block_info),
        ..BatcherConfig::default() // DaType::Blob by default
    };
    let blob_batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, blob_cfg);
    for _ in 4..=6u64 {
        blob_batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        blob_batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
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
