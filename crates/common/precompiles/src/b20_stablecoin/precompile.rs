//! Precompile entry point for the stablecoin B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::BasePrecompileError;

use super::ContractContext;
use crate::{
    B20StablecoinStorage, NoopPrecompileCallObserver, PolicyRegistryStorage, PolicyVersionResolver,
    PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the stablecoin B-20 variant.
///
/// Wraps [`ContractContext`] dispatch behind a [`DynPrecompile`].
#[derive(Debug)]
pub struct B20StablecoinPrecompile;

impl B20StablecoinPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to stablecoin B-20 logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to stablecoin B-20 logic at
    /// `token_address`, gated to the version active at `upgrade`.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(alloc::format!("B20Stablecoin@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            let Some(version) = PolicyVersionResolver::from_base_upgrade(upgrade) else {
                return BasePrecompileError::Revert(Bytes::new()).into_precompile_result(0, 0);
            };
            ContractContext::with_storage_and_policy(
                B20StablecoinStorage::from_address(token_address, ctx),
                PolicyRegistryStorage::new(ctx),
                version,
            )
            .dispatch_with_observer(ctx, &calldata, upgrade, observer)
        })
    }
}
