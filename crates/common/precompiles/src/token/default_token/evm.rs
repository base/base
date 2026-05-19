//! EVM wiring for the `DefaultToken` precompile.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use super::{DefaultToken, storage::DefaultTokenStorage};
use crate::macros::base_precompile;

/// EVM entry point for the `DefaultToken` precompile.
///
/// Wraps [`DefaultToken`] dispatch behind a [`DynPrecompile`] suitable for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct DefaultTokenEvm;

impl DefaultTokenEvm {
    /// Returns a [`DynPrecompile`] that dispatches to the [`DefaultToken`] logic at `token_address`.
    ///
    /// Used by the precompile-lookup fallback to route calls to any B-20 token address.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        base_precompile!(alloc::format!("DefaultToken@{token_address}"), |ctx, calldata| {
            DefaultToken::with_storage(DefaultTokenStorage::from_address(token_address, ctx))
                .dispatch(ctx, &calldata)
        })
    }
}
