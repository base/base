//! Precompile entry point for the `B20Factory`.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};

use crate::{B20FactoryStorage, macros::base_precompile};

/// Entry point for the `B20Factory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct B20Factory;

impl B20Factory {
    /// Installs the singleton `B20Factory` precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles
            .extend_precompiles(core::iter::once((B20FactoryStorage::ADDRESS, Self::precompile())));
    }

    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        base_precompile!("B20Factory", |ctx, calldata| {
            B20FactoryStorage::new(ctx).dispatch(ctx, &calldata)
        })
    }
}
