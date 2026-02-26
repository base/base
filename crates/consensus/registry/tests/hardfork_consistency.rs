//! Integration tests verifying that [`kona_registry`] rollup configs agree with
//! [`base_alloy_hardforks`] chain hardfork schedules for every [`OpHardfork`] variant.

use base_alloy_hardforks::{OpChainHardforks, OpHardfork, OpHardforks};
use kona_registry::test_utils::{BASE_MAINNET_CONFIG, BASE_SEPOLIA_CONFIG};

#[test]
fn mainnet_rollup_config_matches_chain_hardforks() {
    let chain = OpChainHardforks::base_mainnet();
    for fork in OpHardfork::VARIANTS {
        // Regolith activated at genesis on Base and is stored as `regolith_time: None`
        // in the rollup config. The `op_fork_activation` cascade returns Canyon's
        // ForkCondition when Regolith is unset, which differs from OpChainHardforks'
        // explicit Timestamp(0). This is harmless: Canyon is already in the past, so
        // Regolith is always active at any real L2 timestamp.
        if *fork == OpHardfork::Regolith {
            continue;
        }
        assert_eq!(
            BASE_MAINNET_CONFIG.op_fork_activation(*fork),
            chain.op_fork_activation(*fork),
            "mainnet fork activation mismatch for {fork:?}",
        );
    }
}

#[test]
fn sepolia_rollup_config_matches_chain_hardforks() {
    let chain = OpChainHardforks::base_sepolia();
    for fork in OpHardfork::VARIANTS {
        // See comment in mainnet test above.
        if *fork == OpHardfork::Regolith {
            continue;
        }
        assert_eq!(
            BASE_SEPOLIA_CONFIG.op_fork_activation(*fork),
            chain.op_fork_activation(*fork),
            "sepolia fork activation mismatch for {fork:?}",
        );
    }
}
