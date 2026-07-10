//! Precompile entry point for the asset B-20 variant.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use base_common_chains::BaseUpgrade;

use crate::{
    AssetLogic, AssetLogicId, AssetLogicV1, AssetLogicV2, B20AssetStorage, B20AssetToken,
    NoopPrecompileCallObserver, PolicyHandle, PrecompileCallObserver, asset_logic_for,
    macros::base_precompile,
};

/// Entry point for the asset B-20 token precompile.
///
/// Wraps [`B20AssetToken`] dispatch behind a [`DynPrecompile`] for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct B20AssetPrecompile;

impl B20AssetPrecompile {
    /// Returns a [`DynPrecompile`] that dispatches to [`B20AssetToken`] logic at
    /// `token_address`, using the logic version active at `upgrade`.
    pub fn create_precompile(token_address: Address, upgrade: BaseUpgrade) -> DynPrecompile {
        Self::create_precompile_with_observer(token_address, upgrade, NoopPrecompileCallObserver)
    }

    /// Returns a [`DynPrecompile`] that observes and dispatches to [`B20AssetToken`] logic at
    /// `token_address`, using the logic version active at `upgrade`.
    ///
    /// The fork's logic version is resolved once here (at precompile construction); the returned
    /// precompile is monomorphized over that version, so per-call dispatch has no fork branch.
    pub fn create_precompile_with_observer<O>(
        token_address: Address,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        match asset_logic_for(upgrade) {
            AssetLogicId::V1 => Self::build::<O, AssetLogicV1>(token_address, observer),
            AssetLogicId::V2 => Self::build::<O, AssetLogicV2>(token_address, observer),
        }
    }

    /// Builds the dispatch closure monomorphized over a concrete logic version `L`.
    fn build<O, L>(token_address: Address, observer: O) -> DynPrecompile
    where
        O: PrecompileCallObserver,
        L: AssetLogic,
    {
        base_precompile!(alloc::format!("B20AssetToken@{token_address}"), |ctx, calldata| {
            let observer = observer.clone();
            B20AssetToken::<_, _, L>::with_storage_policy_and_logic(
                B20AssetStorage::from_address(token_address, ctx),
                PolicyHandle::new(ctx),
            )
            .dispatch_with_observer(ctx, &calldata, observer)
        })
    }
}
