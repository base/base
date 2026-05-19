//! EVM entry point for the `TokenFactory` precompile.

use alloy_evm::precompiles::DynPrecompile;

use super::storage::TokenFactory;
use crate::macros::base_precompile;

/// EVM entry point for the `TokenFactory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenFactoryEvm;

impl TokenFactoryEvm {
    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("TokenFactory", |ctx, calldata| {
            TokenFactory::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
