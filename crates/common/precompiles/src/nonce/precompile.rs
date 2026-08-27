//! Precompile entry point for the EIP-8130 2D nonce manager.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{NonceManagerStorage, UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the EIP-8130 2D nonce manager precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct NonceManager;

impl NonceManager {
    /// Installs the nonce manager precompile, pinning storage features to `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        precompiles.extend_precompiles(core::iter::once((
            NonceManagerStorage::ADDRESS,
            Self::precompile(upgrade),
        )));
    }

    /// Creates the EVM precompile wrapper with storage features pinned to `upgrade`.
    ///
    /// The wrapper reads `storage_features` from the currently-active fork instead of the macro
    /// default, so a Cobalt-only precompile cannot silently execute under Legacy semantics
    /// (Cantina finding #17).
    pub fn precompile(upgrade: BaseUpgrade) -> DynPrecompile {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            "NonceManager",
            storage_features: storage_features,
            |ctx, calldata| { NonceManagerStorage::new(ctx).dispatch(ctx, &calldata) },
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use base_common_genesis::BaseUpgrade;
    use revm::precompile::Precompiles;

    use super::*;

    #[test]
    fn install_registers_wrapper_at_expected_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        NonceManager::install(&mut precompiles, BaseUpgrade::Cobalt);

        assert!(precompiles.get(&NonceManagerStorage::ADDRESS).is_some());
    }

    #[test]
    fn install_accepts_upgrades_past_cobalt() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        NonceManager::install(&mut precompiles, BaseUpgrade::LATEST);

        assert!(precompiles.get(&NonceManagerStorage::ADDRESS).is_some());
    }
}
