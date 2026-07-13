//! Precompile entry point for the asset B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;

use crate::{
    B20AssetStorage, B20AssetToken, B20AssetVersions, NoopPrecompileCallObserver, PolicyHandle,
    PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the asset B-20 token precompile.
///
/// Wraps [`B20AssetToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20AssetPrecompile;

impl B20AssetPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to the [`B20AssetToken`] logic
    /// active at `upgrade`, or `None` before B-20 activation (pre-Beryl).
    pub fn create_precompile(
        token_address: Address,
        upgrade: BaseUpgrade,
    ) -> Option<DynPrecompile> {
        Self::create_precompile_with_observer(token_address, NoopPrecompileCallObserver, upgrade)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to the
    /// [`B20AssetToken`] logic active at `upgrade`, or `None` before B-20
    /// activation (pre-Beryl).
    ///
    /// The version is resolved once here (at install time) via
    /// [`B20AssetVersions::resolve`] and baked into the precompile closure, so
    /// the per-call dispatch path stays free of fork branching.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        observer: O,
        upgrade: BaseUpgrade,
    ) -> Option<DynPrecompile>
    where
        O: PrecompileCallObserver,
    {
        let version = B20AssetVersions::resolve(upgrade)?;
        Some(base_precompile!(alloc::format!("B20AssetToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_version(ctx, &calldata, observer, version)
        }))
    }
}
