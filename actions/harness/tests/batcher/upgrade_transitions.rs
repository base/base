//! Action tests for the batcher across the Denim block cadence.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

/// A historical Span fixture may derive its pre-Denim prefix, but the cached
/// tail must be discarded before the first Denim block. Resubmitting that tail
/// through the production `SingleBatch` batcher must derive across Denim's 200ms
/// block cadence.
#[tokio::test]
async fn span_batch_stops_at_denim_and_recovers_with_single_batches() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let upgrades = UpgradeConfig {
        regolith_time: Some(0),
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(0),
        jovian_time: Some(0),
        base: BaseUpgradeConfig {
            azul: Some(0),
            beryl: Some(0),
            cobalt: Some(0),
            denim: Some(6),
            ..Default::default()
        },
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    assert_eq!(
        (3..=8).map(|number| rollup_cfg.l2_block_timestamp_parts(number)).collect::<Vec<_>>(),
        vec![(6, 0), (6, 200), (6, 400), (6, 600), (6, 800), (7, 0)]
    );
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut blocks = Vec::new();
    for _ in 0..8 {
        blocks.push(builder.build_next_block_with_single_transaction().await);
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.submit_span_batch_brotli_calldata(&batcher_cfg, &blocks, 100)
        .expect("span fixture submission");
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        2,
        "only blocks before Denim activation at block 3 may derive from the span"
    );

    let mut source = ActionL2Source::new();
    for block in blocks.into_iter().skip(2) {
        source.push(block);
    }
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let recovered = node.run_until_idle().await;
    assert_eq!(recovered, 6, "the SingleBatch path must recover all post-Denim blocks");
    assert_eq!(node.l2_safe_number(), 8, "safe head must advance across Denim");
}
