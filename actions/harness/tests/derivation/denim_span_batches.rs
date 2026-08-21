//! Action tests for span-batch derivation across Denim activation.

use base_action_harness::{
    ActionEngineClient, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_chains::Upgrades;
use base_execution_chainspec::BaseChainSpec;
use base_protocol::{BaseTimeUpdateTx, BatchType};

/// A span crossing Denim activation derives the sequencer's complete 200ms block sequence.
#[tokio::test]
async fn span_batch_crossing_denim_activation_derives() {
    const BLOCK_COUNT: u64 = 8;
    const DENIM_ACTIVATION_TIMESTAMP: u64 = 6;
    const EXPECTED_TIMESTAMPS_MS: [u64; BLOCK_COUNT as usize] =
        [2_000, 4_000, 6_000, 6_200, 6_400, 6_600, 6_800, 7_000];

    let batcher_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .all_forks_active()
        .with_cobalt_at(0)
        .with_denim_at(DENIM_ACTIVATION_TIMESTAMP)
        .build();
    let execution_cfg =
        BaseChainSpec::from_genesis(ActionEngineClient::build_genesis_for_rollup(&rollup_cfg));
    for timestamp in [DENIM_ACTIVATION_TIMESTAMP - 1, DENIM_ACTIVATION_TIMESTAMP] {
        assert_eq!(
            execution_cfg.is_denim_active_at_timestamp(timestamp),
            rollup_cfg.is_denim_active_at_timestamp(timestamp),
            "consensus and execution must agree on Denim activation"
        );
    }
    let mut harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;

    for (block, expected_timestamp_ms) in blocks.iter().zip(EXPECTED_TIMESTAMPS_MS) {
        let actual_timestamp_ms = if block.header.number >= 3 {
            BaseTimeUpdateTx::extract_timestamp_ms(
                &block.body.transactions,
                block.header.number,
                block.header.timestamp,
            )
            .expect("Denim block must contain valid BaseTime metadata")
        } else {
            block.header.timestamp * 1_000
        };
        assert_eq!(actual_timestamp_ms, expected_timestamp_ms);
    }

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg, blocks.clone()).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, BLOCK_COUNT as usize, "all Denim span blocks should derive");
    assert_eq!(node.l2_safe_number(), BLOCK_COUNT, "safe head should reach the span tip");
    for block in blocks {
        assert_eq!(
            node.derived_block_hash(block.header.number).expect("derived block hash"),
            block.header.hash_slow(),
            "derived hash must match the sequencer at block {}",
            block.header.number
        );
    }
}
