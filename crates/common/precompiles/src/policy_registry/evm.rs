//! EVM entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use super::storage::{POLICY_REGISTRY_ADDRESS, PolicyRegistryStorage};
use crate::macros::base_precompile;

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryEvm;

impl PolicyRegistryEvm {
    /// Installs the singleton `PolicyRegistry` precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles
            .extend_precompiles(core::iter::once((POLICY_REGISTRY_ADDRESS, Self::precompile())));
    }

    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("PolicyRegistry", |ctx, calldata| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
