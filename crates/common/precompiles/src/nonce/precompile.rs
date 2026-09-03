//! Precompile entry point for the EIP-8130 2D nonce manager.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{NonceManagerStorage, UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the EIP-8130 2D nonce manager precompile.
///
/// Only installed at Cobalt or later (see `BasePrecompiles::install_with_observer`).
/// The caller passes the active `upgrade` through the constructor, and the wrapper
/// derives its storage features from it via `UpgradeGatedStorageFeatures::from_upgrade`
/// — the same signal every other install site consumes — so features stay locked to
/// the fork that drove the install decision.
#[derive(Debug, Default, Clone, Copy)]
pub struct NonceManager;

impl NonceManager {
    /// Installs the `NonceManager` precompile, gated to the storage features
    /// active at `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        precompiles.extend_precompiles(core::iter::once((
            NonceManagerStorage::ADDRESS,
            Self::precompile(upgrade),
        )));
    }

    /// Creates the EVM precompile wrapper for `NonceManager`, gated to the storage
    /// features active at `upgrade`.
    pub fn precompile(upgrade: BaseUpgrade) -> DynPrecompile {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            "NonceManager",
            storage_features: storage_features,
            |ctx, calldata| NonceManagerStorage::new(ctx).dispatch(ctx, calldata),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use base_common_genesis::BaseUpgrade;
    use revm::precompile::Precompiles;

    use crate::{NonceManager, NonceManagerStorage};

    #[test]
    fn install_places_wrapper_at_storage_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());
        NonceManager::install(&mut precompiles, BaseUpgrade::Cobalt);

        assert!(precompiles.get(&NonceManagerStorage::ADDRESS).is_some());
    }
}
