//! Entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use crate::{PolicyRegistryStorage, macros::base_precompile};

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;

impl PolicyRegistryPrecompile {
    /// Installs the singleton `PolicyRegistry` precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles.extend_precompiles(core::iter::once((
            PolicyRegistryStorage::ADDRESS,
            Self::precompile(),
        )));
    }

    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("PolicyRegistry", |ctx, calldata| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
