//! Action tests for how the batcher closes channels.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// A compressed size target of one byte closes a channel after every block, so each block
/// goes out in its own calldata transaction, and derivation reads them in order.
#[tokio::test]
async fn soft_channel_target_closes_a_channel_per_block() {
    const BLOCK_COUNT: usize = 4;
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            compressed_size_target: Some(1),
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
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT as u64).await;

    Batcher::new(ActionL2Source::from_blocks(blocks.clone()), &h.rollup_config, batcher_cfg)
        .advance(&mut h.l1)
        .await;
    assert_eq!(h.l1.tip().transactions.len(), BLOCK_COUNT, "one channel per block");

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, BLOCK_COUNT);
    assert_eq!(node.l2_safe().block_info.hash, blocks[BLOCK_COUNT - 1].header.hash_slow());
}
