//! Action tests for blob submissions.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::EncoderConfig;
use base_blobs::BlobDecoder;
use base_protocol::Frame;

/// Frames much smaller than a blob share one blob sidecar, and derivation reads the block
/// from it.
#[tokio::test]
async fn small_frames_share_one_blob_and_derive() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .all_forks_active()
        .with_cobalt_at(0)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let batcher = Batcher::new(ActionL2Source::from_blocks([block]), &h.rollup_config, batcher_cfg);
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
