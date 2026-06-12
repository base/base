//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
use base_precompile_macros::precompile;

use crate::{
    ActivationAdminConfig, ActivationRegistryStorage, PrecompileCallObserver,
    macros::base_precompile,
};

/// Entry point for the activation registry precompile.
#[precompile(args(admin_config: ActivationAdminConfig))]
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
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
            Self::precompile(admin_config),
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
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile_with_observer(admin_config, observer),
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
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            let observer = observer.clone();
            ActivationRegistryStorage::new(ctx).dispatch_with_observer(
                ctx,
                &calldata,
                admin_config,
                observer,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Address;
    use revm::precompile::Precompiles;

    use crate::{ActivationRegistry, ActivationRegistryStorage};

    #[test]
    fn install_accepts_static_fallback_admin() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        ActivationRegistry::install(&mut precompiles, Some(Address::repeat_byte(0x11)));

        assert!(precompiles.get(&ActivationRegistryStorage::ADDRESS).is_some());
    }
}
