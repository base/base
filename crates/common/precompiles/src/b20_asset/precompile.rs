//! Precompile entry point for the asset B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::BasePrecompileError;

use crate::{
    B20AssetStorage, B20AssetToken, NoopPrecompileCallObserver, PolicyRegistryStorage,
    PolicyVersions, PrecompileCallObserver, UpgradeGatedStorageFeatures, macros::base_precompile,
};

/// Entry point for the asset B-20 token precompile.
///
/// Wraps [`B20AssetToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20AssetPrecompile;

impl B20AssetPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20AssetToken`] logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20AssetToken`] logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            alloc::format!("B20AssetToken@{token_address}"),
            storage_features: storage_features,
            |ctx, calldata| {
            let observer = observer.clone();
            let Some(version) = PolicyVersions::from_base_upgrade(upgrade) else {
                return ctx.error_result(BasePrecompileError::Revert(Bytes::new()));
            };
            B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(token_address, ctx),
                PolicyRegistryStorage::new(ctx),
                version,
            )
            .dispatch_with_observer(ctx, &calldata, upgrade, observer)
            }
        )
    }
}
