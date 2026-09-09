//! Singular batch ordering and channel recovery.

use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Shared setup helpers for singular batch action tests.
#[derive(Debug)]
struct SingularFixture;

impl SingularFixture {
    /// Returns a calldata batcher configuration for deterministic action tests.
    fn batcher_config() -> BatcherConfig {
        BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..BatcherConfig::default()
        }
    }

    /// Returns a harness with Holocene active from genesis.
    fn post_holocene_harness(batcher: &BatcherConfig) -> ActionTestHarness {
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(batcher).through_holocene().build();
        ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg)
    }
}

/// Post-Holocene strict ordering also drops future singular batches instead of buffering them.
#[tokio::test]
async fn post_holocene_future_singular_is_dropped_not_buffered() {
    let batcher_cfg = SingularFixture::batcher_config();
    let mut harness = SingularFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future post-Holocene singular batch should be dropped");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "only the expected next singular batch should derive");
    assert_eq!(node.l2_safe_number(), 1, "future singular batch must not have been buffered");

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_2]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "resubmitted singular batch should derive after its parent is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance after resubmission");
}

/// Post-Holocene past singular batches are ignored without flushing following batches.
#[tokio::test]
async fn post_holocene_past_singular_does_not_flush_channel() {
    let batcher_cfg = SingularFixture::batcher_config();
    let mut harness = SingularFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1.clone()]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "block 1 should derive before the stale replay");
    assert_eq!(node.l2_safe_number(), 1);

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_1, block_2]).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "stale block 1 should be ignored and block 2 should still derive");
    assert_eq!(node.l2_safe_number(), 2, "post-Holocene past batches must not flush the channel");
}
