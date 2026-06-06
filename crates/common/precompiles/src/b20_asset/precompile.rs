//! Precompile entry point for the asset B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;

use crate::{
    B20AssetStorage, B20AssetToken, NoopPrecompileCallObserver, PolicyHandle,
    PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the asset B-20 token precompile.
///
/// Wraps [`B20AssetToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20AssetPrecompile;

impl B20AssetPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20AssetToken`] logic at
    /// `token_address`.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20AssetToken`] logic at
    /// `token_address`.
    pub fn create_precompile_with_observer<O>(token_address: Address, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20AssetToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
