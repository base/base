//! Harness replay tests for checked-in action fixtures.

use base_action_fixtures::{ActionFixture, ActionFixtureCatalog, DerivationFixtureReplayer};
use base_protocol::AttributesWithParent;

const BASE_MAINNET_DERIVATION_BATCH_FIXTURE: &str =
    "base-mainnet-derivation-batch-l2-4999983-4999983";

#[tokio::test]
async fn captured_l1_data_derives_expected_l2_payloads() {
    let fixture = ActionFixtureCatalog::load("base-mainnet", BASE_MAINNET_DERIVATION_BATCH_FIXTURE)
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
