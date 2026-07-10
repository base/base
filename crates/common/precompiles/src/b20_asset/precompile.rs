//! Precompile entry point for the asset B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use base_common_chains::BaseUpgrade;

use crate::{
    B20AssetStorage, B20AssetToken, NoopPrecompileCallObserver, PolicyHandle,
    PrecompileCallObserver, for_upgrade, macros::base_precompile,
};

/// Entry point for the asset B-20 token precompile.
///
/// Wraps [`B20AssetToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20AssetPrecompile;

impl B20AssetPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20AssetToken`] logic at
    /// `token_address`, running the behavior version active at `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20AssetToken`] logic at
    /// `token_address`, running the behavior version active at `upgrade`.
    ///
    /// The fork's [`crate::Version`] is resolved once here (at precompile construction) and carried
    /// on the token, so per-call dispatch reads a fixed value — no `from_timestamp` on the hot path.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        let version = for_upgrade(upgrade);
        base_precompile!(alloc::format!("B20AssetToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20AssetToken::with_storage_and_policy_versioned(
                B20AssetStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
                version,
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
