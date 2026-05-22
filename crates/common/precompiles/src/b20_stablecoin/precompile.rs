//! Precompile entry point for the stablecoin B-20 variant.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;

use super::{B20StablecoinToken, storage::B20StablecoinStorage};
use crate::{PolicyHandle, TokenVariant, macros::base_precompile};

/// Entry point for the stablecoin B-20 token precompile.
///
/// Wraps [`B20StablecoinToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20StablecoinPrecompile;

impl B20StablecoinPrecompile {
    /// Installs the stablecoin dynamic precompile lookup into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap) {
        precompiles.set_precompile_lookup(Self::lookup);
    }

    /// Returns the stablecoin precompile for `address`, if it encodes a stablecoin token.
    pub fn lookup(address: &Address) -> Option<DynPrecompile> {
        match TokenVariant::from_address(*address)? {
            TokenVariant::Stablecoin => Some(Self::create_precompile(*address)),
            _ => None,
        }
    }

    /// Returns a [`DynPrecompile`] that dispatches to [`B20StablecoinToken`] logic at
    /// `token_address`.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        base_precompile!(alloc::format!("B20StablecoinToken@{token_address}"), |ctx, calldata| {
            B20StablecoinToken::with_storage_and_policy(
                B20StablecoinStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch(ctx, &calldata)
        })
    }
}
