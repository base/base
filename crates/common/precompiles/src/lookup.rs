//! Dynamic lookup for Beryl-native precompiles.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;

use crate::{B20SecurityPrecompile, B20StablecoinPrecompile, B20TokenPrecompile, B20Variant};

/// Dynamic precompile lookup installed for Beryl and later forks.
#[derive(Debug, Default, Clone, Copy)]
pub struct BerylLookup;

impl BerylLookup {
    /// Installs the Beryl dynamic precompile lookup into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles.set_precompile_lookup(Self::lookup);
    }

    /// Returns the B-20 variant precompile for `address`, if it encodes one.
    pub fn lookup(address: &Address) -> Option<DynPrecompile> {
        match B20Variant::from_address(*address)? {
            B20Variant::B20 => Some(B20TokenPrecompile::create_precompile(*address)),
            B20Variant::Stablecoin => Some(B20StablecoinPrecompile::create_precompile(*address)),
            B20Variant::Security => Some(B20SecurityPrecompile::create_precompile(*address)),
        }
    }
}
