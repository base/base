//! Dynamic lookup for Beryl-native precompiles.

use alloy_evm::precompiles::{DynPrecompile, PrecompileLookup, PrecompilesMap};
use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;

use crate::{
    B20AssetPrecompile, B20StablecoinPrecompile, B20Variant, NoopPrecompileCallObserver,
    PrecompileCallObserver,
};

/// Dynamic precompile lookup installed for Beryl and later forks.
#[derive(Debug, Default, Clone, Copy)]
pub struct BerylLookup;

impl BerylLookup {
    /// Installs the Beryl dynamic precompile lookup into `precompiles`, routing
    /// B-20 token versions for `upgrade`.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        Self::install_with_observer(precompiles, NoopPrecompileCallObserver, upgrade);
    }

    /// Installs the Beryl dynamic precompile lookup with an observer into
    /// `precompiles`, routing B-20 token versions for `upgrade`.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        observer: O,
        upgrade: BaseUpgrade,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.set_precompile_lookup(BerylLookupWithObserver::new(observer, upgrade));
    }

    /// Returns the B-20 variant precompile for `address` under `upgrade`, if it
    /// encodes one.
    pub fn lookup(address: &Address, upgrade: BaseUpgrade) -> Option<DynPrecompile> {
        Self::lookup_with_observer(address, NoopPrecompileCallObserver, upgrade)
    }

    /// Returns an observed B-20 variant precompile for `address` under `upgrade`,
    /// if it encodes one.
    pub fn lookup_with_observer<O>(
        address: &Address,
        observer: O,
        upgrade: BaseUpgrade,
    ) -> Option<DynPrecompile>
    where
        O: PrecompileCallObserver,
    {
        match B20Variant::from_address(*address)? {
            B20Variant::Stablecoin => Some(
                B20StablecoinPrecompile::create_precompile_with_observer(
                    *address, observer, upgrade,
                ),
            ),
            B20Variant::Asset => {
                Some(B20AssetPrecompile::create_precompile_with_observer(*address, observer, upgrade))
            }
        }
    }
}

/// Dynamic Beryl precompile lookup with an observer.
#[derive(Debug, Clone)]
pub struct BerylLookupWithObserver<O> {
    observer: O,
    upgrade: BaseUpgrade,
}

impl<O> BerylLookupWithObserver<O> {
    /// Creates a Beryl dynamic precompile lookup with `observer`, routing B-20
    /// token versions for `upgrade`.
    pub const fn new(observer: O, upgrade: BaseUpgrade) -> Self {
        Self { observer, upgrade }
    }
}

impl<O> PrecompileLookup for BerylLookupWithObserver<O>
where
    O: PrecompileCallObserver,
{
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        BerylLookup::lookup_with_observer(address, self.observer.clone(), self.upgrade)
    }
}
