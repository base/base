//! Entry point for the `PolicyRegistry` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::StorageSemantics;

use crate::{
    NoopPrecompileCallObserver, PolicyRegistryStorage, PrecompileCallObserver,
    macros::base_precompile,
};

/// EVM entry point for the `PolicyRegistry` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;

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
        Self::install_with_observer_and_storage_semantics(
            precompiles,
            upgrade,
            observer,
            if upgrade >= BaseUpgrade::Cobalt {
                StorageSemantics::Cobalt
            } else {
                StorageSemantics::Legacy
            },
        );
    }

    /// Installs the `PolicyRegistryPrecompile` precompile with storage semantics.
    pub fn install_with_observer_and_storage_semantics<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
        storage_semantics: StorageSemantics,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            PolicyRegistryStorage::ADDRESS,
            Self::precompile_with_observer_and_storage_semantics(
                upgrade,
                observer,
                storage_semantics,
            ),
        )));
    }

    /// Creates the EVM precompile wrapper for `PolicyRegistryPrecompile`, gated to the
    /// version active at `upgrade`.
    pub fn precompile_with_observer<O>(upgrade: BaseUpgrade, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        Self::precompile_with_observer_and_storage_semantics(
            upgrade,
            observer,
            if upgrade >= BaseUpgrade::Cobalt {
                StorageSemantics::Cobalt
            } else {
                StorageSemantics::Legacy
            },
        )
    }

    /// Creates the EVM precompile wrapper with storage semantics.
    pub fn precompile_with_observer_and_storage_semantics<O>(
        upgrade: BaseUpgrade,
        observer: O,
        storage_semantics: StorageSemantics,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(
            "PolicyRegistryPrecompile",
            storage_semantics: storage_semantics,
            |ctx, calldata| {
            let observer = observer.clone();
            PolicyRegistryStorage::new(ctx)
                .dispatch_with_observer(ctx, &calldata, upgrade, observer)
            }
        )
    }
}
