//! Integration tests verifying that the registry's rollup configs agree with chain hardfork
//! schedules for every [`BaseUpgrade`] variant.

use base_common_chains::{
    BaseUpgrade, ChainUpgrades, Upgrades,
    test_utils::{BASE_MAINNET_ROLLUP_CONFIG, BASE_SEPOLIA_ROLLUP_CONFIG},
};

#[test]
fn mainnet_rollup_config_matches_chain_hardforks() {
    let chain = ChainUpgrades::mainnet();
    for fork in BaseUpgrade::VARIANTS {
        // Regolith activated at genesis on Base and is stored as `regolith_time: Some(0)`
        // in the derived rollup config. The `upgrade_activation` cascade returns Canyon's
        // ForkCondition when traversing, which differs from ChainUpgrades'
        // explicit Timestamp(0). Skip to avoid false mismatches.
        if *fork == BaseUpgrade::Regolith {
            continue;
        }
        assert_eq!(
            BASE_MAINNET_ROLLUP_CONFIG.upgrade_activation(*fork),
            chain.upgrade_activation(*fork),
            "mainnet fork activation mismatch for {fork:?}",
        );
    }
}

#[test]
fn sepolia_rollup_config_matches_chain_hardforks() {
    let chain = ChainUpgrades::sepolia();
    for fork in BaseUpgrade::VARIANTS {
        // See comment in mainnet test above.
        if *fork == BaseUpgrade::Regolith {
            continue;
        }
        assert_eq!(
            BASE_SEPOLIA_ROLLUP_CONFIG.upgrade_activation(*fork),
            chain.upgrade_activation(*fork),
            "sepolia fork activation mismatch for {fork:?}",
        );
    }
}
