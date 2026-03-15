#![doc = "Stress tests for batcher channel fragmentation."]

use base_action_harness::{ActionL2Source, ActionTestHarness, BatcherConfig, SharedL1Chain};
use base_batcher_encoder::EncoderConfig;

/// Stress test: tiny `max_frame_size` forces channel fragmentation into many frames.
///
/// With `max_frame_size = 80`, each L2 block's compressed batch data is split
/// across multiple frames, producing far more L1 transactions than blocks. This
/// exercises the multi-frame submission path and verifies the encoder correctly
/// fragments large channels.
#[test]
fn batcher_stress_large_txs() {
    let mut h = ActionTestHarness::default();
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 5 L2 blocks.
    let mut source = ActionL2Source::new();
    for _ in 0..5 {
        source.push(sequencer.build_next_block().expect("build L2 block"));
    }

    // Tiny max_frame_size forces fragmentation: at 80 bytes per frame,
    // 5 L2 blocks (each with a deposit tx) produce many frames.
    let cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let mut batcher = h.create_batcher(source, cfg);
    let frames = batcher.encode_frames().expect("encode frames");

    // With max_frame_size=80 and 5 blocks, expect multiple frames.
    assert!(
        frames.len() >= 3,
        "expected at least 3 frames with max_frame_size=80, got {}",
        frames.len()
    );

    // Submit all frames to L1.
    batcher.submit_frames(&frames);
    drop(batcher);

    // Mine the block containing all frame txs.
    h.l1.mine_block();
    let tip = h.l1.tip();

    // Each frame becomes one batcher tx.
    assert_eq!(tip.batcher_txs.len(), frames.len(), "mined block should contain one tx per frame");
}
