//! Precompile entry point for the EIP-8130 transaction context.

use base_common_genesis::BaseUpgrade;
use base_precompile_macros::precompile;

use crate::{TxContextStorage, UpgradeGatedStorageFeatures};

/// Entry point for the EIP-8130 transaction context precompile.
///
/// Only installed at Cobalt or later (see `BasePrecompiles::install_with_observer`),
/// so the wrapper pins its storage features to at least Cobalt while riding
/// `BaseUpgrade::LATEST` when LATEST already meets that floor.
#[precompile(
    install,
    storage_features = UpgradeGatedStorageFeatures::at_least(BaseUpgrade::Cobalt),
)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TxContext;

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use revm::precompile::Precompiles;

    use crate::{TxContext, TxContextStorage};

    // Feature-selection verification chain: see the sibling module
    // `crate::nonce::precompile::tests` for the full argument.
    #[test]
    fn install_places_wrapper_at_storage_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());
        TxContext::install(&mut precompiles);

        assert!(precompiles.get(&TxContextStorage::ADDRESS).is_some());
    }
}
