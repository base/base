//! Dynamic lookup for Beryl-native precompiles.

use alloy_evm::precompiles::{DynPrecompile, PrecompileLookup, PrecompilesMap};
use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::StorageSemantics;

use crate::{
    B20AssetPrecompile, B20StablecoinPrecompile, B20Variant, BaseStorageSemantics,
    NoopPrecompileCallObserver, PrecompileCallObserver,
};

/// Dynamic precompile lookup installed for Beryl and later forks.
#[derive(Debug, Default, Clone, Copy)]
pub struct BerylLookup;

impl BerylLookup {
    /// Installs the Beryl dynamic precompile lookup into `precompiles` for `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        Self::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
    }

    /// Installs the Beryl dynamic precompile lookup with an observer into `precompiles` for
    /// `upgrade`.
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
            BaseStorageSemantics::from_upgrade(upgrade),
        );
    }

    /// Installs the Beryl dynamic precompile lookup with storage semantics.
    pub fn install_with_observer_and_storage_semantics<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
        storage_semantics: StorageSemantics,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.set_precompile_lookup(BerylLookupWithObserver::new_with_storage_semantics(
            observer,
            upgrade,
            storage_semantics,
        ));
    }

    /// Returns the B-20 variant precompile for `address` at `upgrade`, if it encodes one.
    pub fn lookup(address: &Address, upgrade: BaseUpgrade) -> Option<DynPrecompile> {
        Self::lookup_with_observer(address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns an observed B-20 variant precompile for `address` at `upgrade`, if it encodes one.
    ///
    /// The active version is resolved inside the token's dispatcher from `upgrade`; the lookup
    /// only forwards the fork.
    pub fn lookup_with_observer<O>(
        address: &Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> Option<DynPrecompile>
    where
        O: PrecompileCallObserver,
    {
        Self::lookup_with_observer_and_storage_semantics(
            address,
            upgrade,
            observer,
            BaseStorageSemantics::from_upgrade(upgrade),
        )
    }

    /// Returns an observed B-20 variant precompile with storage semantics.
    pub fn lookup_with_observer_and_storage_semantics<O>(
        address: &Address,
        upgrade: BaseUpgrade,
        observer: O,
        storage_semantics: StorageSemantics,
    ) -> Option<DynPrecompile>
    where
        O: PrecompileCallObserver,
    {
        match B20Variant::from_address(*address)? {
            B20Variant::Stablecoin => Some(
                B20StablecoinPrecompile::create_precompile_with_observer_and_storage_semantics(
                    *address,
                    upgrade,
                    observer,
                    storage_semantics,
                ),
            ),
            B20Variant::Asset => {
                Some(B20AssetPrecompile::create_precompile_with_observer_and_storage_semantics(
                    *address,
                    upgrade,
                    observer,
                    storage_semantics,
                ))
            }
        }
    }
}

/// Dynamic Beryl precompile lookup with an observer.
#[derive(Debug, Clone)]
pub struct BerylLookupWithObserver<O> {
    observer: O,
    upgrade: BaseUpgrade,
    storage_semantics: StorageSemantics,
}

impl<O> BerylLookupWithObserver<O> {
    /// Creates a Beryl dynamic precompile lookup with `observer` for `upgrade`.
    pub const fn new(observer: O, upgrade: BaseUpgrade) -> Self {
        let storage_semantics = BaseStorageSemantics::from_upgrade(upgrade);
        Self::new_with_storage_semantics(observer, upgrade, storage_semantics)
    }

    /// Creates a Beryl dynamic precompile lookup with `observer` and storage semantics.
    pub const fn new_with_storage_semantics(
        observer: O,
        upgrade: BaseUpgrade,
        storage_semantics: StorageSemantics,
    ) -> Self {
        Self { observer, upgrade, storage_semantics }
    }
}

impl<O> PrecompileLookup for BerylLookupWithObserver<O>
where
    O: PrecompileCallObserver,
{
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        BerylLookup::lookup_with_observer_and_storage_semantics(
            address,
            self.upgrade,
            self.observer.clone(),
            self.storage_semantics,
        )
    }
}
