//! Precompile entry point for the EIP-8130 2D nonce manager.

use base_common_genesis::BaseUpgrade;
use base_precompile_macros::precompile;

use crate::{NonceManagerStorage, UpgradeGatedStorageFeatures};

/// Entry point for the EIP-8130 2D nonce manager precompile.
///
/// Only installed at Cobalt or later (see `BasePrecompiles::install_with_observer`),
/// so the wrapper pins its storage features to at least Cobalt while riding
/// `BaseUpgrade::LATEST` when LATEST already meets that floor.
#[precompile(
    install,
    storage_features = UpgradeGatedStorageFeatures::at_least(BaseUpgrade::Cobalt),
)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NonceManager;

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use revm::precompile::Precompiles;

    use crate::{NonceManager, NonceManagerStorage};

    // End-to-end feature selection is verified by the three-part chain:
    //   1. `UpgradeGatedStorageFeatures::at_least(Cobalt) == Cobalt`
    //      (see `crate::spec::tests::at_least_honors_floor_when_latest_is_behind`).
    //   2. `#[precompile(storage_features = ...)]` threads the expression into
    //      `base_precompile!` (see
    //      `base_precompile_macros::precompile::tests::storage_features_arg_threads_through_expansion`).
    //   3. `base_precompile!` passes it into `EvmPrecompileStorageProvider::new_with_storage_features`
    //      (see `crate::macros`).
    //
    // The smoke test below only exercises install placement — the source-level
    // annotation is what pins the feature.
    #[test]
    fn install_places_wrapper_at_storage_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());
        NonceManager::install(&mut precompiles);

        assert!(precompiles.get(&NonceManagerStorage::ADDRESS).is_some());
    }
}
