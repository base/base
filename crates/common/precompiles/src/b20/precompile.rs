//! Precompile entry point for the `B20Token`.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use super::{B20Token, storage::B20TokenStorage};
use crate::{PolicyHandle, macros::base_precompile};

/// Entry point for the `B20Token` precompile.
///
/// Wraps [`B20Token`] dispatch behind a [`DynPrecompile`] suitable for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20TokenPrecompile;

impl B20TokenPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to the [`B20Token`] logic at `token_address`.
    ///
    /// Used by the precompile-lookup fallback to route calls to any B-20 token address.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        base_precompile!(alloc::format!("B20Token@{token_address}"), |ctx, calldata| {
            B20Token::with_storage_and_policy(
                B20TokenStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch(ctx, &calldata)
        })
    }
}
