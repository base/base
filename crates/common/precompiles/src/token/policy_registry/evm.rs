//! EVM entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::DynPrecompile;

use super::storage::PolicyRegistryStorage;
use crate::macros::base_precompile;

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryEvm;

impl PolicyRegistryEvm {
    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("PolicyRegistry", |ctx, calldata| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
