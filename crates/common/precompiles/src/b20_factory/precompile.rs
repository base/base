//! Precompile entry point for the `B20Factory`.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_precompile_macros::precompile;

use crate::{B20FactoryStorage, PrecompileCallObserver, macros::base_precompile};

/// Entry point for the `B20Factory` precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct B20Factory;

impl B20Factory {
    /// Installs the `B20Factory` precompile with an observer.
    pub fn install_with_observer<O>(precompiles: &mut PrecompilesMap, observer: O)
    where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            B20FactoryStorage::ADDRESS,
            Self::precompile_with_observer(observer),
        )));
    }

    /// Creates the EVM precompile wrapper for `B20Factory` with an observer.
    pub fn precompile_with_observer<O>(observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!("B20Factory", |ctx, calldata| {
            let observer = observer.clone();
            B20FactoryStorage::new(ctx).dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
