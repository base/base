//! Dynamic lookup for Beryl-native precompiles.

use std::sync::OnceLock;

use alloy_evm::precompiles::{DynPrecompile, PrecompileLookup, PrecompilesMap};
use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;

use crate::{
    B20AssetPrecompile, B20StablecoinPrecompile, B20Variant, NoopPrecompileCallObserver,
    PrecompileCallObserver,
};

/// Environment variable that, when set to a hex address, forces [`BerylLookup`] to return `None`
/// for that address so EVM bytecode deployed there executes instead of the native precompile.
///
/// This exists solely for benchmarking the deployed Solidity B-20 reference implementation against
/// the native precompile at an identical storage layout. It must never be set in production.
pub const B20_PRECOMPILE_EXCLUDE_ENV: &str = "B20_PRECOMPILE_EXCLUDE_ADDRESS";

/// Caches the parsed exclusion address so the hot lookup path avoids repeated env reads.
static EXCLUDED_ADDRESS: OnceLock<Option<Address>> = OnceLock::new();

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
        precompiles.set_precompile_lookup(BerylLookupWithObserver::new(observer, upgrade));
    }

    /// Returns the benchmark-only excluded address parsed from [`B20_PRECOMPILE_EXCLUDE_ENV`].
    pub fn excluded_address() -> Option<Address> {
        *EXCLUDED_ADDRESS.get_or_init(|| {
            std::env::var(B20_PRECOMPILE_EXCLUDE_ENV)
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
    }

    /// Returns whether `address` is excluded from native B-20 dispatch for benchmarking.
    pub fn is_excluded(address: &Address) -> bool {
        Self::excluded_address() == Some(*address)
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
        if Self::is_excluded(address) {
            return None;
        }

        match B20Variant::from_address(*address)? {
            B20Variant::Stablecoin => {
                Some(B20StablecoinPrecompile::create_precompile_with_observer(
                    *address, upgrade, observer,
                ))
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
    observer: O,
    upgrade: BaseUpgrade,
}

impl<O> BerylLookupWithObserver<O> {
    /// Creates a Beryl dynamic precompile lookup with `observer` for `upgrade`.
    pub const fn new(observer: O, upgrade: BaseUpgrade) -> Self {
        Self { observer, upgrade }
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
