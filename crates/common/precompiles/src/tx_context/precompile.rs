//! Precompile entry point for the EIP-8130 transaction context.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{TxContextStorage, UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the EIP-8130 transaction context precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct TxContext;

impl TxContext {
    /// Installs the tx context precompile, pinning storage features to `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        precompiles.extend_precompiles(core::iter::once((
            TxContextStorage::ADDRESS,
            Self::precompile(upgrade),
        )));
    }

    /// Creates the EVM precompile wrapper with storage features pinned to `upgrade`.
    ///
    /// The wrapper reads `storage_features` from the currently-active fork instead of the macro
    /// default, so a Cobalt-only precompile cannot silently execute under Legacy semantics.
    pub fn precompile(upgrade: BaseUpgrade) -> DynPrecompile {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            "TxContext",
            storage_features: storage_features,
            |ctx, calldata| { TxContextStorage::new(ctx).dispatch(ctx, &calldata) },
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

        TxContext::install(&mut precompiles, BaseUpgrade::Cobalt);

        assert!(precompiles.get(&TxContextStorage::ADDRESS).is_some());
    }
}
