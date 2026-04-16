//! Derivation test across the Base Azul activation boundary.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Derives 4 L2 blocks across the Base Azul activation boundary (ts=4, block 2)
/// and asserts each block includes 1 user transaction.
#[tokio::test]
async fn azul_derivation_crosses_activation_boundary() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    // All Optimism forks through Jovian active from genesis; Base Azul at ts=4.
    // With block_time=2 and L2 genesis at ts=0:
    //   block 1 → ts=2  (pre-Base Azul)
    //   block 2 → ts=4  (first Base Azul block)
    //   block 3 → ts=6  (post-Base Azul)
    //   block 4 → ts=8  (post-Base Azul)
    let base_azul_time = 4u64;
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .with_azul_at(base_azul_time)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    node.initialize().await;

    for i in 1..=4u64 {
        batcher.push_block(builder.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
        let derived = node.run_until_idle().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");

        let block = node.derived_block(i).expect("derived block must be recorded");
        assert_eq!(block.user_tx_count, 1, "L2 block {i} should contain 1 user transaction");
    }

    assert_eq!(
        node.l2_safe().block_info.number,
        4,
        "safe head should advance past the Base Azul activation boundary"
    );
}
