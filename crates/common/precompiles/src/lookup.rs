//! Dynamic lookup for Beryl-native precompiles.

use alloy_evm::precompiles::{DynPrecompile, PrecompileLookup, PrecompilesMap};
use alloy_primitives::Address;
use base_common_chains::BaseUpgrade;

use crate::{
    B20AssetPrecompile, B20StablecoinPrecompile, B20Variant, NoopPrecompileCallObserver,
    PrecompileCallObserver,
};

/// Dynamic precompile lookup installed for Beryl and later forks.
#[derive(Debug, Default, Clone, Copy)]
pub struct BerylLookup;

impl BerylLookup {
    /// Installs the Beryl dynamic precompile lookup into `precompiles` for the given `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        Self::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
    }

    /// Installs the Beryl dynamic precompile lookup with an observer into `precompiles`.
    ///
    /// `upgrade` is captured so each resolved B-20 token precompile runs the logic version active
    /// at the block being executed.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        upgrade: BaseUpgrade,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.set_precompile_lookup(BerylLookupWithObserver::new(upgrade, observer));
    }

    /// Returns the B-20 variant precompile for `address`, if it encodes one.
    pub fn lookup(address: &Address, upgrade: BaseUpgrade) -> Option<DynPrecompile> {
        Self::lookup_with_observer(address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns an observed B-20 variant precompile for `address`, if it encodes one.
    pub fn lookup_with_observer<O>(
        address: &Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> Option<DynPrecompile>
    where
        O: PrecompileCallObserver,
    {
        match B20Variant::from_address(*address)? {
            B20Variant::Stablecoin => {
                Some(B20StablecoinPrecompile::create_precompile_with_observer(*address, observer))
            }
            B20Variant::Asset => Some(B20AssetPrecompile::create_precompile_with_observer(
                *address, upgrade, observer,
            )),
        }
    }
}

/// Dynamic Beryl precompile lookup with an observer.
#[derive(Debug, Clone)]
pub struct BerylLookupWithObserver<O> {
    upgrade: BaseUpgrade,
    observer: O,
}

impl<O> BerylLookupWithObserver<O> {
    /// Creates a Beryl dynamic precompile lookup for `upgrade` with `observer`.
    pub const fn new(upgrade: BaseUpgrade, observer: O) -> Self {
        Self { upgrade, observer }
    }
}

impl<O> PrecompileLookup for BerylLookupWithObserver<O>
where
    O: PrecompileCallObserver,
{
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        BerylLookup::lookup_with_observer(address, self.upgrade, self.observer.clone())
    }
}
