//! Harness replay tests for checked-in action fixtures.

use base_action_fixtures::{ActionFixtureAdapter, ActionFixtureCatalog, DerivationFixtureReplayer};

const BASE_MAINNET_DERIVATION_BATCH_FIXTURE: &str =
    "base-mainnet-derivation-batch-l2-4999983-4999983";

#[tokio::test]
async fn captured_l1_data_derives_expected_l2_payloads() {
    let fixture = ActionFixtureCatalog::load("base-mainnet", BASE_MAINNET_DERIVATION_BATCH_FIXTURE)
        .expect("fixture must load");
    let payloads =
        DerivationFixtureReplayer::derive_payloads(&fixture).await.expect("fixture derives");
    let derivation = fixture.derivation.as_ref().expect("fixture records derivation anchor");
    let rollup_config = DerivationFixtureReplayer::rollup_config(&fixture)
        .expect("fixture network has rollup config");
    let l1_end = fixture.manifest.l1_end.expect("fixture records L1 end");
    let mut expected_parent = derivation.safe_head;

    assert_eq!(payloads.len(), fixture.l2_blocks.len(), "derived payload count");
    for (payload, fixture_block) in payloads.iter().zip(&fixture.l2_blocks) {
        let derived_from = payload.derived_from().expect("payload records L1 origin");
        let l1_origin = fixture_block.l1_origin.expect("fixture L2 block records L1 origin");

        assert_eq!(payload.block_number(), fixture_block.header.number);
        assert_eq!(payload.parent, expected_parent);
        assert_eq!(payload.attributes.payload_attributes.timestamp, fixture_block.header.timestamp);
        assert!((l1_origin.number..=l1_end.number).contains(&derived_from.number));

        let transactions =
            payload.attributes.transactions.as_ref().expect("payload carries transactions");
        assert_eq!(transactions, &fixture_block.transactions);

        expected_parent =
            ActionFixtureAdapter::l2_block_info(fixture_block, &rollup_config.genesis)
                .expect("fixture L2 block converts to cursor");
    }
}
