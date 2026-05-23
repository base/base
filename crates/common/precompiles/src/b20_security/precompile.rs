//! Precompile entry point for the security B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use super::{B20SecurityToken, storage::B20SecurityStorage};
use crate::{PolicyHandle, macros::base_precompile};

/// Entry point for the security B-20 token precompile.
///
/// Wraps [`B20SecurityToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20SecurityPrecompile;

impl B20SecurityPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20SecurityToken`] logic at
    /// `token_address`.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        base_precompile!(alloc::format!("B20SecurityToken@{token_address}"), |ctx, calldata| {
            B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch(ctx, &calldata)
        })
    }
}
