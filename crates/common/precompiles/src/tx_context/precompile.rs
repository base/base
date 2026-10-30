//! Precompile entry point for the EIP-8130 transaction context.

#[cfg(feature = "revm")]
use alloy_evm::precompiles::PrecompilesMap;
#[cfg(feature = "revm")]
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{PrecompileResult, StorageCtx};

use crate::TxContextStorage;
#[cfg(feature = "revm")]
use crate::{UpgradeGatedStorageFeatures, macros::base_precompile};

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
    /// Precompile identifier reported to the execution engine.
    pub const ID: &'static str = "TxContext";

    /// Dispatches one `TxContext` call.
    pub fn run(ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        TxContextStorage::new(ctx).dispatch(ctx, calldata)
    }
}

#[cfg(feature = "revm")]
impl TxContext {
    /// Installs the `TxContext` precompile, gated to the storage features
    /// active at `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        let precompile = base_precompile!(
            Self::ID,
            storage_features: storage_features,
            |ctx, calldata| Self::run(ctx, calldata),
        );
        precompiles.extend_precompiles(core::iter::once((TxContextStorage::ADDRESS, precompile)));
    }
}

#[cfg(all(test, feature = "revm"))]
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
