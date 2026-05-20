//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use super::ActivationRegistryStorage;
use crate::macros::base_precompile;

/// Entry point for the activation registry precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Installs the singleton activation registry precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile(),
        )));
    }

    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn precompile() -> DynPrecompile {
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            ActivationRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
