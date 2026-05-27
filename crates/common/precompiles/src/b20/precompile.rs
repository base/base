//! Precompile entry point for the `B20Token`.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use crate::{
    B20Token, B20TokenStorage, NoopPrecompileCallObserver, PolicyHandle, PrecompileCallObserver,
    macros::base_precompile,
};

/// Entry point for the `B20Token` precompile.
///
/// Wraps [`B20Token`] dispatch behind a [`DynPrecompile`] suitable for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20TokenPrecompile;

impl B20TokenPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to the [`B20Token`] logic at `token_address`.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to the [`B20Token`] logic at
    /// `token_address`.
    pub fn create_precompile_with_observer<O>(token_address: Address, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20Token@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20Token::with_storage_and_policy(
                B20TokenStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
