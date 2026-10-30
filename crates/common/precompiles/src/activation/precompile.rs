//! Precompile entry point for the activation registry.

#[cfg(feature = "revm")]
use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{PrecompileResult, StorageCtx};

use crate::{ActivationAdminConfig, ActivationRegistryStorage, PrecompileCallObserver};
#[cfg(feature = "revm")]
use crate::{NoopPrecompileCallObserver, UpgradeGatedStorageFeatures, macros::base_precompile};

/// Entry point for the activation registry precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Precompile identifier reported to the execution engine.
    pub const ID: &'static str = "ActivationRegistry";

    /// Observes and dispatches one activation registry call under `admin_config`, gated to the
    /// version active at `upgrade`.
    pub fn run<O>(
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        ActivationRegistryStorage::new(ctx).dispatch_with_observer(
            ctx,
            calldata,
            admin_config,
            upgrade,
            observer,
        )
    }
}

#[cfg(feature = "revm")]
impl ActivationRegistry {
    /// Installs the activation registry precompile with an explicit admin configuration.
    pub fn install_with_config(
        precompiles: &mut PrecompilesMap,
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
    ) {
        Self::install_with_observer(precompiles, admin_config, upgrade, NoopPrecompileCallObserver);
    }

    /// Installs the activation registry precompile with an observer.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile_with_observer(admin_config, upgrade, observer),
        )));
    }

    /// Creates the revm precompile wrapper for the activation registry with an observer.
    pub fn precompile_with_observer<O>(
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        let storage_features = UpgradeGatedStorageFeatures::from_upgrade(upgrade);
        base_precompile!(
            Self::ID,
            storage_features: storage_features,
            |ctx, calldata| Self::run(ctx, calldata, admin_config, upgrade, observer.clone())
        )
    }
}

#[cfg(all(test, feature = "revm"))]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Address;
    use base_common_genesis::BaseUpgrade;
    use revm::precompile::Precompiles;

    use crate::{
        ActivationAdminConfig, ActivationRegistry, ActivationRegistryStorage,
        NoopPrecompileCallObserver,
    };

    #[test]
    fn install_accepts_static_fallback_admin() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        ActivationRegistry::install_with_observer(
            &mut precompiles,
            ActivationAdminConfig::static_fallback(Some(Address::repeat_byte(0x11))),
            BaseUpgrade::Beryl,
            NoopPrecompileCallObserver,
        );

        assert!(precompiles.get(&ActivationRegistryStorage::ADDRESS).is_some());
    }
}
