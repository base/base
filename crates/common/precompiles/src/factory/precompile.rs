//! Precompile entry point for the `TokenFactory`.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use super::storage::TokenFactory;
use crate::macros::base_precompile;

/// Entry point for the `TokenFactory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenFactoryPrecompile;

impl TokenFactoryPrecompile {
    /// Installs the singleton `TokenFactory` precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles
            .extend_precompiles(core::iter::once((TokenFactory::ADDRESS, Self::precompile())));
    }

    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("TokenFactory", |ctx, calldata| {
            TokenFactory::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
