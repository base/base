//! Precompile entry point for the EIP-8130 transaction context.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{TxContextStorage, UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the EIP-8130 transaction context precompile.
///
/// Only installed at Cobalt or later (see `BasePrecompiles::install_with_observer`).
/// The caller passes the active `upgrade` through the constructor, and the wrapper
/// derives its storage features from it via `UpgradeGatedStorageFeatures::from_upgrade`
/// — the same signal every other install site consumes — so features stay locked to
/// the fork that drove the install decision.
#[derive(Debug, Default, Clone, Copy)]
pub struct TxContext;

impl TxContext {
    /// Installs the `TxContext` precompile, gated to the storage features active
    /// at `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        precompiles.extend_precompiles(core::iter::once((
            TxContextStorage::ADDRESS,
            Self::precompile(upgrade),
        )));
    }

    /// Creates the EVM precompile wrapper for `TxContext`, gated to the storage
    /// features active at `upgrade`.
    pub fn precompile(upgrade: BaseUpgrade) -> DynPrecompile {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            "TxContext",
            storage_features: storage_features,
            |ctx, calldata| TxContextStorage::new(ctx).dispatch(ctx, calldata),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use base_common_genesis::BaseUpgrade;
    use revm::precompile::Precompiles;

    use crate::{TxContext, TxContextStorage};

    #[test]
    fn install_places_wrapper_at_storage_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());
        TxContext::install(&mut precompiles, BaseUpgrade::Cobalt);

        assert!(precompiles.get(&TxContextStorage::ADDRESS).is_some());
    }
}
