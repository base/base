//! Precompile entry point for the stablecoin B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::BasePrecompileError;

use crate::{
    B20StablecoinStorage, B20StablecoinToken, NoopPrecompileCallObserver, PolicyRegistryStorage,
    PolicyVersions, PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the stablecoin B-20 variant.
///
/// Wraps [`B20StablecoinToken`] dispatch behind a [`DynPrecompile`].
#[derive(Debug)]
pub struct B20StablecoinPrecompile;

impl B20StablecoinPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20StablecoinToken`] logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20StablecoinToken`] logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20StablecoinToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            let Some(version) = PolicyVersions::from_base_upgrade(upgrade) else {
                return BasePrecompileError::Revert(Bytes::new()).into_precompile_result(0, 0);
            };
            B20StablecoinToken::with_storage_and_policy(
                B20StablecoinStorage::from_address(token_address, ctx),
                PolicyRegistryStorage::new(ctx),
                version,
            )
            .dispatch_with_observer(ctx, &calldata, upgrade, observer)
        })
    }
}
