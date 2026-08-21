//! Integration tests verifying that the derived rollup configs agree with chain upgrade
//! schedules for every [`BaseUpgrade`] variant.

use base_common_chains::{
    ChainUpgrades, Upgrades,
    test_utils::{BASE_MAINNET_ROLLUP_CONFIG, BASE_SEPOLIA_ROLLUP_CONFIG},
};
use base_common_genesis::BaseUpgrade;

#[test]
fn mainnet_rollup_config_matches_chain_upgrades() {
    let chain = ChainUpgrades::mainnet();
    for fork in BaseUpgrade::VARIANTS {
        assert_eq!(
            BASE_MAINNET_ROLLUP_CONFIG.fork_condition(*fork),
            chain.fork_condition(*fork),
            "mainnet fork activation mismatch for {fork:?}",
        );
    }
}

#[test]
fn sepolia_rollup_config_matches_chain_upgrades() {
    let chain = ChainUpgrades::sepolia();
    for fork in BaseUpgrade::VARIANTS {
        assert_eq!(
            BASE_SEPOLIA_ROLLUP_CONFIG.fork_condition(*fork),
            chain.fork_condition(*fork),
            "sepolia fork activation mismatch for {fork:?}",
        );
    }
}
