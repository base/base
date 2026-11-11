//! Precompile entry point for the `B20Factory`.

#[cfg(feature = "revm")]
use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{PrecompileResult, StorageCtx};

use crate::{B20FactoryStorage, PrecompileCallObserver};
#[cfg(feature = "revm")]
use crate::{UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the `B20Factory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct B20Factory;

impl B20Factory {
    /// Precompile identifier reported to the execution engine.
    pub const ID: &'static str = "B20Factory";

    /// Observes and dispatches one `B20Factory` call, gated to the version active at `upgrade`.
    pub fn run<O>(
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        B20FactoryStorage::new(ctx).dispatch_with_observer(ctx, calldata, upgrade, observer)
    }
}

#[cfg(feature = "revm")]
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

    /// Creates the revm precompile wrapper for `B20Factory` with an observer, gated to the
    /// version active at `upgrade`.
    pub fn precompile_with_observer<O>(upgrade: BaseUpgrade, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            Self::ID,
            storage_features: storage_features,
            |ctx, calldata| Self::run(ctx, calldata, upgrade, observer.clone())
        )
    }
}
