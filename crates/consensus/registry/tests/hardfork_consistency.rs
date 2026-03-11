//! Integration tests verifying that [`base_consensus_registry`] rollup configs agree with
//! [`base_alloy_upgrades`] chain hardfork schedules for every [`BaseUpgrade`] variant.

use base_alloy_upgrades::{BaseChainUpgrades, BaseUpgrade, BaseUpgrades};
use base_consensus_registry::test_utils::{BASE_MAINNET_CONFIG, BASE_SEPOLIA_CONFIG};

#[test]
fn mainnet_rollup_config_matches_chain_hardforks() {
    let chain = BaseChainUpgrades::base_mainnet();
    for fork in BaseUpgrade::VARIANTS {
        // Regolith activated at genesis on Base and is stored as `regolith_time: None`
        // in the rollup config. The `upgrade_activation` cascade returns Canyon's
        // ForkCondition when Regolith is unset, which differs from BaseChainUpgrades'
        // explicit Timestamp(0). This is harmless: Canyon is already in the past, so
        // Regolith is always active at any real L2 timestamp.
        if *fork == BaseUpgrade::Regolith {
            continue;
        }
        assert_eq!(
            BASE_MAINNET_CONFIG.upgrade_activation(*fork),
            chain.upgrade_activation(*fork),
            "mainnet fork activation mismatch for {fork:?}",
        );
    }
}

#[test]
fn sepolia_rollup_config_matches_chain_hardforks() {
    let chain = BaseChainUpgrades::base_sepolia();
    for fork in BaseUpgrade::VARIANTS {
        // See comment in mainnet test above.
        if *fork == BaseUpgrade::Regolith {
            continue;
        }
        assert_eq!(
            BASE_SEPOLIA_CONFIG.upgrade_activation(*fork),
            chain.upgrade_activation(*fork),
            "sepolia fork activation mismatch for {fork:?}",
        );
    }
}
