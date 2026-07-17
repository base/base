//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
use base_precompile_macros::precompile;
use base_precompile_storage::StorageSemantics;

use crate::{
    ActivationAdminConfig, ActivationRegistryStorage, NoopPrecompileCallObserver,
    PrecompileCallObserver, macros::base_precompile,
};

/// Entry point for the activation registry precompile.
#[precompile(args(admin_config: ActivationAdminConfig))]
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Returns the storage semantics implied by the activation admin configuration.
    pub const fn storage_semantics(admin_config: ActivationAdminConfig) -> StorageSemantics {
        if admin_config.state_enabled { StorageSemantics::Cobalt } else { StorageSemantics::Legacy }
    }

    /// Installs the activation registry precompile using a static fallback admin.
    pub fn install(precompiles: &mut PrecompilesMap, activation_admin_address: Option<Address>) {
        Self::install_with_config(
            precompiles,
            ActivationAdminConfig::static_fallback(activation_admin_address),
        );
    }

    /// Installs the activation registry precompile with an explicit admin configuration.
    pub fn install_with_config(
        precompiles: &mut PrecompilesMap,
        admin_config: ActivationAdminConfig,
    ) {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile_with_observer_and_storage_semantics(
                admin_config,
                NoopPrecompileCallObserver,
                Self::storage_semantics(admin_config),
            ),
        )));
    }

    /// Installs the activation registry precompile with an observer.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        admin_config: ActivationAdminConfig,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        Self::install_with_observer_and_storage_semantics(
            precompiles,
            admin_config,
            observer,
            Self::storage_semantics(admin_config),
        );
    }

    /// Installs the activation registry precompile with storage semantics.
    pub fn install_with_observer_and_storage_semantics<O>(
        precompiles: &mut PrecompilesMap,
        admin_config: ActivationAdminConfig,
        observer: O,
        storage_semantics: StorageSemantics,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile_with_observer_and_storage_semantics(
                admin_config,
                observer,
                storage_semantics,
            ),
        )));
    }

    /// Creates the EVM precompile wrapper for the activation registry with an observer.
    pub fn precompile_with_observer<O>(
        admin_config: ActivationAdminConfig,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        Self::precompile_with_observer_and_storage_semantics(
            admin_config,
            observer,
            Self::storage_semantics(admin_config),
        )
    }

    /// Creates the EVM precompile wrapper for the activation registry with storage semantics.
    pub fn precompile_with_observer_and_storage_semantics<O>(
        admin_config: ActivationAdminConfig,
        observer: O,
        storage_semantics: StorageSemantics,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!(
            "ActivationRegistry",
            storage_semantics: storage_semantics,
            |ctx, calldata| {
            let observer = observer.clone();
            ActivationRegistryStorage::new(ctx).dispatch_with_observer(
                ctx,
                &calldata,
                admin_config,
                observer,
            )
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Address;
    use base_precompile_storage::StorageSemantics;
    use revm::precompile::Precompiles;

    use crate::{ActivationAdminConfig, ActivationRegistry, ActivationRegistryStorage};

    #[test]
    fn install_accepts_static_fallback_admin() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        ActivationRegistry::install(&mut precompiles, Some(Address::repeat_byte(0x11)));

        assert!(precompiles.get(&ActivationRegistryStorage::ADDRESS).is_some());
    }

    #[test]
    fn admin_config_selects_storage_semantics() {
        assert_eq!(
            ActivationRegistry::storage_semantics(ActivationAdminConfig::static_fallback(None)),
            StorageSemantics::Legacy,
        );
        assert_eq!(
            ActivationRegistry::storage_semantics(ActivationAdminConfig::state_backed(None)),
            StorageSemantics::Cobalt,
        );
    }
}
