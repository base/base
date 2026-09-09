//! Azul baseline rules apply regardless of retired fork timestamps.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoding_channel::{DaType, EncoderConfig};
use base_common_chain_config::UpgradeConfig;
use base_consensus_batch_types::L1BlockInfoTx;

#[tokio::test]
async fn retired_fork_timestamps_do_not_change_consensus_attributes() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..Default::default() },
        ..Default::default()
    };
    let upgrades = UpgradeConfig {
        ecotone_time: Some(6),
        isthmus_time: Some(6),
        jovian_time: Some(6),
        ..Default::default()
    };
    let mut cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    let system = cfg.genesis.system_config.as_mut().unwrap();
    system.operator_fee_scalar = Some(1200);
    system.operator_fee_constant = Some(400);
    let mut harness = ActionTestHarness::new(L1MinerConfig::default(), cfg);
    let mut sequencer =
        harness.create_l2_sequencer(SharedL1Chain::from_blocks(harness.l1.chain().to_vec()));
    let mut batcher = Batcher::new(ActionL2Source::new(), &harness.rollup_config, batcher_cfg);
    for _ in 0..4 {
        let block = sequencer.build_next_block_with_single_transaction().await;
        assert_eq!(
            block.body.transactions.len(),
            2,
            "only L1 info and the user transaction are present"
        );
        let deposit = block.body.transactions[0].as_deposit().unwrap();
        let info = L1BlockInfoTx::decode_calldata(&deposit.input).unwrap();
        assert!(matches!(info, L1BlockInfoTx::Jovian(_)));
        assert_eq!(info.operator_fee_scalar(), 1200);
        assert_eq!(info.operator_fee_constant(), 400);
        batcher.push_block(block);
        batcher.advance(&mut harness.l1).await;
    }
    let (mut node, _) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 4);
    assert_eq!(node.l2_safe_number(), 4);
}
