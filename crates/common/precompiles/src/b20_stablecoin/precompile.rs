//! Precompile entry point for the stablecoin B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;

use crate::{
    B20StablecoinStorage, B20StablecoinToken, NoopPrecompileCallObserver, PolicyHandle,
    PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the stablecoin B-20 token precompile.
///
/// Wraps [`B20StablecoinToken`] dispatch behind a [`DynPrecompile`].
#[derive(Debug)]
pub struct B20StablecoinPrecompile;

impl B20StablecoinPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20StablecoinToken`] logic at
    /// `token_address` for `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, NoopPrecompileCallObserver, upgrade)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20StablecoinToken`] logic at
    /// `token_address` for `upgrade`.
    ///
    /// The stablecoin variant is not version-seamed in this change, so `_upgrade`
    /// is accepted for signature parity with the asset variant but intentionally
    /// unused — behavior is identical across forks.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        observer: O,
        _upgrade: BaseUpgrade,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20StablecoinToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20StablecoinToken::with_storage_and_policy(
                B20StablecoinStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
