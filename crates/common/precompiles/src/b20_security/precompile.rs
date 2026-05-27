//! Precompile entry point for the security B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use super::{B20SecurityToken, storage::B20SecurityStorage};
use crate::{
    NoopPrecompileCallObserver, PolicyHandle, PrecompileCallObserver, macros::base_precompile,
};

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
        Self::create_precompile_with_observer(token_address, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20SecurityToken`] logic at
    /// `token_address`.
    pub fn create_precompile_with_observer<O>(token_address: Address, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20SecurityToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
