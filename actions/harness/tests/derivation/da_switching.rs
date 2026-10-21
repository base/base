//! Action tests for batches of one sender that switches from calldata to blobs.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Batches one sender posts as calldata, then as blobs, derive in order.
#[tokio::test]
async fn calldata_then_blob_batches_derive_in_order() {
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&BatcherConfig::default())
        .all_forks_active()
        .with_cobalt_at(0)
        .build();
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

    assert_eq!(node.run_until_idle().await, 6, "the calldata and blob batches derive");
    assert_eq!(node.l2_safe_number(), 6, "safe head should reach L2 block 6");
}
