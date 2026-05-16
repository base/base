//! Harness replay tests for checked-in action fixtures.

use base_action_fixtures::{
    ActionFixture, ActionFixtureAdapter, ActionFixtureCatalog, DerivationFixtureReplayer,
};
use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_protocol::AttributesWithParent;

#[tokio::test]
async fn captured_l1_data_derives_expected_l2_payloads() {
    let fixture =
        ActionFixtureCatalog::load("base-mainnet", "base-mainnet-derivation-window-l2-1-1")
            .expect("fixture must load");
    let mut rollup_config =
        DerivationFixtureReplayer::rollup_config(&fixture).expect("fixture has rollup config");

    rollup_config.seq_window_size = 2;

    assert_fixture_payloads(
        &fixture,
        DerivationFixtureReplayer::derive_payloads_with_rollup_config(&fixture, rollup_config)
            .await
            .expect("fixture derives with shortened sequence window"),
    );
}

#[tokio::test]
#[ignore = "exact Base mainnet genesis replay advances through the 3600-block sequence window"]
async fn captured_l1_data_derives_expected_l2_payloads_exact_mainnet() {
    let fixture =
        ActionFixtureCatalog::load("base-mainnet", "base-mainnet-derivation-window-l2-1-1")
            .expect("fixture must load");
    let payloads =
        DerivationFixtureReplayer::derive_payloads(&fixture).await.expect("fixture derives");

    assert_fixture_payloads(&fixture, payloads);
}

fn assert_fixture_payloads(fixture: &ActionFixture, payloads: Vec<AttributesWithParent>) {
    let derivation = fixture.derivation.as_ref().expect("fixture records derivation anchor");

    assert_eq!(payloads.len(), fixture.l2_blocks.len());
    for (payload, fixture_block) in payloads.iter().zip(&fixture.l2_blocks) {
        let derived_from = payload.derived_from().expect("payload records L1 origin");
        let l1_origin = fixture_block.l1_origin.expect("fixture L2 block records L1 origin");
        let l1_end = fixture.manifest.l1_end.expect("fixture records L1 end");
        let transactions =
            payload.attributes.transactions.as_ref().expect("payload carries transactions");

        assert_eq!(payload.block_number(), fixture_block.header.number);
        assert_eq!(payload.parent.block_info, derivation.safe_head.block_info);
        assert_eq!(payload.attributes.payload_attributes.timestamp, fixture_block.header.timestamp);
        assert_eq!(l1_origin.number, derivation.safe_head.l1_origin.number);
        assert_eq!(l1_origin.hash, derivation.safe_head.l1_origin.hash);
        assert!(derived_from.number >= l1_origin.number);
        assert!(derived_from.number <= l1_end.number);
        assert_eq!(transactions, &fixture_block.transactions);
    }
}

#[test]
fn captured_l2_block_replays_through_harness_unsafe_path() {
    let fixture =
        ActionFixtureCatalog::load("base-mainnet", "base-mainnet-derivation-window-l2-1-1")
            .expect("fixture must load");
    let fixture_block = fixture.l2_blocks.first().expect("fixture must contain one L2 block");
    let block = ActionFixtureAdapter::l2_block(fixture_block).expect("fixture L2 block decodes");

    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain.clone());
    let transport = harness.create_supervised_p2p(&mut sequencer);
    let mut node = harness.create_test_rollup_node(&sequencer, l1_chain, transport);

    node.act_l2_unsafe_gossip_receive(&block);

    let expected_safe_head = fixture.expected.safe_head.expect("fixture records expected head");
    assert_eq!(node.l2_unsafe_number(), expected_safe_head.number);
    assert_eq!(node.l2_unsafe().block_info.hash, expected_safe_head.hash);
    assert_eq!(node.l2_unsafe().block_info.parent_hash, fixture_block.header.parent_hash);
    assert_eq!(node.l2_unsafe().block_info.timestamp, fixture_block.header.timestamp);
}
