//! Harness replay tests for checked-in action fixtures.

use base_action_fixtures::{
    ActionFixtureAdapter, ActionFixtureCatalog, DerivationFixtureReplayer, FixtureKind,
};

#[tokio::test]
async fn checked_in_derivation_fixtures_replay_expected_l2_payloads() {
    let entries = ActionFixtureCatalog::list().expect("fixture catalog lists");
    let mut replayed = 0usize;

    for entry in entries {
        let fixture = entry.load().unwrap_or_else(|error| {
            panic!("fixture {}/{} must load: {error}", entry.network, entry.name)
        });

        if fixture.manifest.kind != FixtureKind::Derivation {
            continue;
        }
        replayed += 1;

        let payloads = DerivationFixtureReplayer::derive_payloads(&fixture)
            .await
            .unwrap_or_else(|error| panic!("fixture {} derives: {error}", fixture.manifest.name));
        let derivation = fixture.derivation.as_ref().unwrap_or_else(|| {
            panic!("fixture {} records derivation anchor", fixture.manifest.name)
        });
        let rollup_config =
            DerivationFixtureReplayer::rollup_config(&fixture).unwrap_or_else(|error| {
                panic!("fixture {} has rollup config: {error}", fixture.manifest.name)
            });
        let l1_end = fixture
            .manifest
            .l1_end
            .unwrap_or_else(|| panic!("fixture {} records L1 end", fixture.manifest.name));
        let mut expected_parent = derivation.safe_head;

        assert_eq!(
            payloads.len(),
            fixture.l2_blocks.len(),
            "fixture {} derived payload count",
            fixture.manifest.name
        );
        for (payload, fixture_block) in payloads.iter().zip(&fixture.l2_blocks) {
            let derived_from = payload.derived_from().unwrap_or_else(|| {
                panic!("fixture {} payload records L1 origin", fixture.manifest.name)
            });
            let l1_origin = fixture_block.l1_origin.unwrap_or_else(|| {
                panic!("fixture {} L2 block records L1 origin", fixture.manifest.name)
            });

            assert_eq!(payload.block_number(), fixture_block.header.number);
            assert_eq!(payload.parent, expected_parent);
            assert_eq!(
                payload.attributes.payload_attributes.timestamp,
                fixture_block.header.timestamp
            );
            assert!((l1_origin.number..=l1_end.number).contains(&derived_from.number));

            let transactions = payload.attributes.transactions.as_ref().unwrap_or_else(|| {
                panic!("fixture {} payload carries transactions", fixture.manifest.name)
            });
            assert_eq!(transactions, &fixture_block.transactions);

            expected_parent =
                ActionFixtureAdapter::l2_block_info(fixture_block, &rollup_config.genesis)
                    .unwrap_or_else(|error| {
                        panic!(
                            "fixture {} L2 block converts to cursor: {error}",
                            fixture.manifest.name
                        )
                    });
        }
    }

    assert!(replayed > 0, "expected at least one checked-in derivation fixture");
}
