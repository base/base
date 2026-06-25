//! Entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_precompile_macros::precompile;

use crate::{PolicyRegistryStorage, PrecompileCallObserver, macros::base_precompile};

/// EVM entry point for the `PolicyRegistry` precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;

impl PolicyRegistryPrecompile {
    /// Installs the `PolicyRegistryPrecompile` precompile with an observer.
    pub fn install_with_observer<O>(precompiles: &mut PrecompilesMap, observer: O)
    where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            PolicyRegistryStorage::ADDRESS,
            Self::precompile_with_observer(observer),
        )));
    }

    /// Creates the EVM precompile wrapper for `PolicyRegistryPrecompile` with an observer.
    pub fn precompile_with_observer<O>(observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!("PolicyRegistryPrecompile", |ctx, calldata| {
            let observer = observer.clone();
            PolicyRegistryStorage::new(ctx).dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
