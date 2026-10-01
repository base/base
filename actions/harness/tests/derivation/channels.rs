//! Action tests for a channel whose frames land in several L1 blocks.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// A channel whose frames are spread across two consecutive L1 blocks derives once its last
/// frame lands.
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
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .all_forks_active()
        .with_cobalt_at(0)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let batcher = Batcher::new(ActionL2Source::from_blocks([block]), &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;
    let frame_count = batcher.pending_count();
    assert!(frame_count >= 2, "the block must span several frames");

    // L1 block 1 carries frame 0 only.
    batcher.stage_n_submissions(&mut h.l1, 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    node.initialize().await;
    node.run_until_idle().await;
    assert_eq!(node.l2_safe_number(), 0, "the channel is incomplete");

    // L1 block 2 carries the rest, well within the channel timeout.
    batcher.stage_n_submissions(&mut h.l1, frame_count - 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    assert_eq!(node.run_until_idle().await, 1, "the complete channel derives the block");
    assert_eq!(node.l2_safe_number(), 1);
}
