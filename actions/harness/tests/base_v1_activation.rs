//! Action tests for Base V1 (Osaka) hardfork activation.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};

/// Derives 4 L2 blocks across the Base V1 activation boundary (ts=4, block 2)
/// and asserts each block includes 1 user transaction.
#[tokio::test]
async fn base_v1_derivation_crosses_activation_boundary() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    // All Optimism forks through Jovian active from genesis; Base V1 at ts=4.
    // With block_time=2 and L2 genesis at ts=0:
    //   block 1 → ts=2  (pre-Base V1)
    //   block 2 → ts=4  (first Base V1 block)
    //   block 3 → ts=6  (post-Base V1)
    //   block 4 → ts=8  (post-Base V1)
    let base_v1_time = 4u64;
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .with_base_v1_at(base_v1_time)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 1..=4u64 {
        batcher.push_block(builder.build_next_block());
        batcher.advance(&mut h.l1).await;
    }

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await;

    for i in 1..=4u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await;
        let derived = verifier.act_l2_pipeline_full().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");

        let block = verifier.derived_block(i).expect("derived block must be recorded");
        assert_eq!(block.user_tx_count, 1, "L2 block {i} should contain 1 user transaction");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "safe head should advance past the Base V1 activation boundary"
    );
}
