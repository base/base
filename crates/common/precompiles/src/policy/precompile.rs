//! Entry point for the `PolicyRegistry` precompile.

#[cfg(feature = "revm")]
use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{PrecompileResult, StorageCtx};

#[cfg(feature = "revm")]
use crate::{NoopPrecompileCallObserver, UpgradeGatedStorageFeatures, macros::base_precompile};
use crate::{PolicyRegistryStorage, PrecompileCallObserver};

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;

impl PolicyRegistryPrecompile {
    /// Precompile identifier reported to the execution engine.
    pub const ID: &'static str = "PolicyRegistryPrecompile";

    /// Observes and dispatches one `PolicyRegistry` call, gated to the version active at
    /// `upgrade`.
    pub fn run<O>(
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        PolicyRegistryStorage::new(ctx).dispatch_with_observer(ctx, calldata, upgrade, observer)
    }
}

#[cfg(feature = "revm")]
impl PolicyRegistryPrecompile {
    /// Installs the `PolicyRegistryPrecompile` precompile, gated to the version active at
    /// `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        Self::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
    }

    /// Installs the `PolicyRegistryPrecompile` precompile with an observer, gated to the
    /// version active at `upgrade`.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            PolicyRegistryStorage::ADDRESS,
            Self::precompile_with_observer(upgrade, observer),
        )));
    }

    /// Creates the revm precompile wrapper for `PolicyRegistryPrecompile`, gated to the
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
