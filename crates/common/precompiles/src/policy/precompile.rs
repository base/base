//! Entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{
    NoopPrecompileCallObserver, PolicyRegistryStorage, PrecompileCallObserver,
    macros::base_precompile,
};

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;

impl PolicyRegistryPrecompile {
    /// Installs the `PolicyRegistryPrecompile` precompile, gated to the version active at
    /// `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        Self::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
    }

    /// Installs the `PolicyRegistryPrecompile` precompile with an observer, gated to the
    /// version active at `upgrade`.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            PolicyRegistryStorage::ADDRESS,
            Self::precompile_with_observer(upgrade, observer),
        )));
    }

    /// Creates the EVM precompile wrapper for `PolicyRegistryPrecompile`, gated to the
    /// version active at `upgrade`.
    pub fn precompile_with_observer<O>(upgrade: BaseUpgrade, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!("PolicyRegistryPrecompile", |ctx, calldata| {
            let observer = observer.clone();
            PolicyRegistryStorage::new(ctx)
                .dispatch_with_observer(ctx, &calldata, upgrade, observer)
        })
    }
}
