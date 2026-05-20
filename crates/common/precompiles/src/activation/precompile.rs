//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use super::ActivationRegistry;
use crate::macros::base_precompile;

/// Entry point for the activation registry precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistryPrecompile;

impl ActivationRegistryPrecompile {
    /// Installs the singleton activation registry precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistry::ADDRESS,
            Self::precompile(),
        )));
    }

    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn precompile() -> DynPrecompile {
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            ActivationRegistry::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
