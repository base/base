//! Precompile entry point for the `B20Factory`.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;

use crate::{B20FactoryStorage, PrecompileCallObserver, macros::base_precompile};

/// Entry point for the `B20Factory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct B20Factory;

impl B20Factory {
    /// Installs the `B20Factory` precompile with an observer, gated to the version active
    /// at `upgrade`.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            B20FactoryStorage::ADDRESS,
            Self::precompile_with_observer(upgrade, observer),
        )));
    }

    /// Creates the EVM precompile wrapper for `B20Factory` with an observer, gated to the
    /// version active at `upgrade`.
    pub fn precompile_with_observer<O>(upgrade: BaseUpgrade, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!("B20Factory", |ctx, calldata| {
            let observer = observer.clone();
            B20FactoryStorage::new(ctx).dispatch_with_observer(ctx, &calldata, upgrade, observer)
        })
    }
}
