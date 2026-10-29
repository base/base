//! Precompile entry point for the stablecoin B-20 variant.

#[cfg(feature = "revm")]
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, PrecompileResult, StorageCtx};

use crate::{
    B20StablecoinStorage, B20StablecoinToken, PolicyRegistryStorage, PolicyVersions,
    PrecompileCallObserver,
};
#[cfg(feature = "revm")]
use crate::{UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the stablecoin B-20 variant.
#[derive(Debug)]
pub struct B20StablecoinPrecompile;

impl B20StablecoinPrecompile {
    /// Observes and dispatches one call to the [`B20StablecoinToken`] at `token_address`, gated
    /// to the version active at `upgrade`.
    pub fn run<O>(
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let Some(version) = PolicyVersions::from_base_upgrade(upgrade) else {
            return BasePrecompileError::Revert(Bytes::new()).into_precompile_result(0, 0);
        };
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(token_address, ctx),
            PolicyRegistryStorage::new(ctx),
            version,
        )
        .dispatch_with_observer(ctx, calldata, upgrade, observer)
    }
}

#[cfg(feature = "revm")]
impl B20StablecoinPrecompile {
    /// Returns a revm [`DynPrecompile`] that observes and dispatches to [`B20StablecoinToken`]
    /// logic at `token_address`, gated to the version active at `upgrade`.
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
            alloc::format!("B20StablecoinToken@{token_address}"),
            storage_features: storage_features,
            |ctx, calldata| Self::run(ctx, calldata, token_address, upgrade, observer.clone())
        )
    }
}
